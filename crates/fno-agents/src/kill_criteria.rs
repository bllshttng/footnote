//! `fno-agents kill-check` verb — Rust port of `scripts/lib/kill-criteria.sh`
//! (packaging EPIC ab-8bdb4642, eliminate-don't-vendor leg).
//!
//! Provable byte-parity with the bash `check_kill_criteria <plan_path>`:
//! the bash script stays in-tree as the parity oracle (differential tests in
//! `tests/kill_criteria_parity.rs`) until a separate agent re-points the
//! Python `fno phase kill-check` caller at this binary.
//!
//! Contract (matched exactly):
//!   - exit 0 + empty stdout when nothing fired (no criteria, none fired, or a
//!     malformed predicate that was WARN-skipped).
//!   - exit 1 + exactly one stdout line `KILL_CRITERIA_FIRED <name>|<reason>`
//!     when a predicate fired.
//!   - a MALFORMED/unknown predicate logs `kill-criteria: WARN: ...` to stderr
//!     and is skipped (does NOT abort; exit stays 0). When the manifest exists,
//!     the bash also appends a `## Kill Criteria Warning` block to it — the
//!     Rust port reproduces that side-effect faithfully.
//!
//! Inputs:
//!   - positional `plan_path` (a single plan `*.md` file, full-mode or
//!     quick-plan).
//!   - env `STATE_FILE` (override for target-state.md, else
//!     `<git-root>/.fno/target-state.md`). `--state-file` is also accepted so a
//!     caller / test can pin it explicitly without an env var.
//!   - env `FNO_KILLCHECK_GIT_BIN` (test override for the `git` binary, mirrors
//!     loopcheck's `FNO_LOOPCHECK_GIT_BIN`); `--git-bin` flag also accepted.
//!
//! The four predicates and the manifest/plan parsing reproduce the awk/sed/grep
//! logic of the bash line-for-line; see the per-fn comments for the exact bash
//! site each mirrors.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Parsed kill_criteria entry. Empty fields are preserved (the bash read -r
/// keeps empty name/predicate/reason and validates them in the main loop).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    idx: usize,
    name: String,
    predicate: String,
    reason: String,
}

/// Resolved CLI/env inputs for one kill-check run.
struct KillCheckArgs {
    plan_path: Option<String>,
    /// `STATE_FILE` env / `--state-file` override. PARITY NOTE: in the bash,
    /// this ONLY redirects `_kc_log_warn`'s append destination (line 45). The
    /// actual field reads (`_kc_state_field` / `_kc_consecutive_failures`)
    /// IGNORE `STATE_FILE` and always derive `<git-root>/.fno/target-state.md`,
    /// because no caller passes them the optional explicit-path argument. We
    /// reproduce that split faithfully: `state_file` feeds only the warn path.
    state_file: Option<String>,
    git_bin: String,
}

fn parse_args(args: &[String]) -> KillCheckArgs {
    let mut plan_path: Option<String> = None;
    let mut state_file = std::env::var("STATE_FILE").ok().filter(|s| !s.is_empty());
    let mut git_bin = std::env::var("FNO_KILLCHECK_GIT_BIN").unwrap_or_else(|_| "git".to_string());

    // Tolerate a leading `kill-check` verb token (loopcheck does the same with
    // `loop-check`). `bin/client.rs` already passes `&args[1..]`, but callers /
    // tests that include the verb still parse correctly.
    let args = if args.first().map(|s| s.as_str()) == Some("kill-check") {
        &args[1..]
    } else {
        args
    };

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if let Some(v) = arg.strip_prefix("--state-file=") {
            state_file = Some(v.to_string());
        } else if arg == "--state-file" {
            i += 1;
            if let Some(v) = args.get(i) {
                state_file = Some(v.clone());
            }
        } else if let Some(v) = arg.strip_prefix("--git-bin=") {
            git_bin = v.to_string();
        } else if arg == "--git-bin" {
            i += 1;
            if let Some(v) = args.get(i) {
                git_bin = v.clone();
            }
        } else if !arg.starts_with("--") && plan_path.is_none() {
            plan_path = Some(arg.clone());
        }
        i += 1;
    }

    KillCheckArgs {
        plan_path,
        state_file,
        git_bin,
    }
}

/// Captured side-effect output of one run, so the public entry can print and
/// the test entry can assert without spawning a process.
struct RunOutput {
    code: i32,
    stdout: String,
    stderr: String,
}

/// `git rev-parse --show-toplevel`, falling back to cwd (mirrors the bash
/// `git rev-parse --show-toplevel 2>/dev/null || pwd`).
fn git_root(git_bin: &str) -> PathBuf {
    let out = Command::new(git_bin)
        .args(["rev-parse", "--show-toplevel"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
            } else {
                PathBuf::from(s)
            }
        }
        _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

/// State file for the WARN-append side-effect (`_kc_log_warn`, bash line 45):
/// `STATE_FILE` env / `--state-file` wins; else `<git-root>/.fno/target-state.md`.
fn resolve_warn_state_file(args: &KillCheckArgs) -> PathBuf {
    if let Some(sf) = &args.state_file {
        return PathBuf::from(sf);
    }
    git_root(&args.git_bin).join(".fno/target-state.md")
}

/// State file for the FIELD reads (`_kc_state_field` /
/// `_kc_consecutive_failures`). PARITY: the bash always derives
/// `<git-root>/.fno/target-state.md` here and never consults `STATE_FILE`, so
/// neither do we — `state_file` (the warn override) is deliberately not read.
fn resolve_field_state_file(args: &KillCheckArgs) -> PathBuf {
    git_root(&args.git_bin).join(".fno/target-state.md")
}

// ── helpers (mirror the bash `_kc_*` functions) ───────────────────────────────

/// `_kc_log_warn`: stderr line + (when the state file exists) an appended
/// `## Kill Criteria Warning` block. Faithfully reproduces the bash side-effect,
/// including the UTC timestamp. Best-effort: an append failure is swallowed
/// exactly like the bash `|| true`.
fn log_warn(out: &mut RunOutput, args: &KillCheckArgs, msg: &str) {
    out.stderr
        .push_str(&format!("kill-criteria: WARN: {msg}\n"));
    let state_file = resolve_warn_state_file(args);
    if state_file.is_file() {
        let block = format!(
            "\n## Kill Criteria Warning\n- {}: {}\n",
            utc_timestamp(),
            msg
        );
        // `>> "$state_file" 2>/dev/null || true` — best-effort append.
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&state_file) {
            let _ = f.write_all(block.as_bytes());
        }
    }
}

/// `date -u +%Y-%m-%dT%H:%M:%SZ`. Reuses the same civil-from-unix conversion
/// shape loopcheck/events use so the format is identical (no chrono clock).
fn utc_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = (rem / 3600) as u32;
    let min = ((rem % 3600) / 60) as u32;
    let sec = (rem % 60) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// `_kc_read_plan_block`: extract the kill_criteria block text from a plan.
/// Returns None when the plan path doesn't resolve OR no block is present.
fn read_plan_block(plan_path: &str) -> Option<String> {
    let plan = Path::new(plan_path);
    if !plan.is_file() {
        return None;
    }

    let content = std::fs::read_to_string(plan).ok()?;

    // Full-mode: frontmatter (between the first two `---` lines) -> the
    // kill_criteria: block.
    let fm = extract_frontmatter(&content);
    let fm_block = extract_kill_criteria_block(&fm);
    if !fm_block.is_empty() {
        return Some(fm_block);
    }

    // Quick-mode: fenced YAML under `## Kill Criteria` -> the kill_criteria:
    // block.
    let fenced = extract_fenced_under_heading(&content);
    let fenced_block = extract_kill_criteria_block(&fenced);
    if !fenced_block.is_empty() {
        return Some(fenced_block);
    }

    None
}

/// Mirror of the first awk in `_kc_read_plan_block`:
///   /^---/ { c++; if (c==2) exit; next }  c==1 { print }
/// Collects the lines strictly between the first and second `---` lines.
fn extract_frontmatter(content: &str) -> String {
    let mut c = 0;
    let mut lines: Vec<&str> = Vec::new();
    for line in content.lines() {
        if line.starts_with("---") {
            c += 1;
            if c == 2 {
                break;
            }
            continue;
        }
        if c == 1 {
            lines.push(line);
        }
    }
    join_lines(&lines)
}

/// Mirror of the quick-mode awk:
///   /^## Kill Criteria[[:space:]]*$/ { found=1; next }
///   found && /^## / { exit }
///   found && /^```/ { in_fence=!in_fence; next }
///   found && in_fence { print }
fn extract_fenced_under_heading(content: &str) -> String {
    let mut found = false;
    let mut in_fence = false;
    let mut lines: Vec<&str> = Vec::new();
    for line in content.lines() {
        if !found {
            // `/^## Kill Criteria[[:space:]]*$/`: heading then only whitespace.
            if let Some(rest) = line.strip_prefix("## Kill Criteria") {
                if rest.chars().all(|c| c == ' ' || c == '\t') {
                    found = true;
                    continue;
                }
            }
            continue;
        }
        // found:
        if line.starts_with("## ") {
            break;
        }
        if line.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            lines.push(line);
        }
    }
    join_lines(&lines)
}

/// Mirror of the second awk in `_kc_read_plan_block` (run on the frontmatter or
/// fenced text):
///   /^kill_criteria:/ { in_block=1; next }
///   in_block && /^[A-Za-z_][A-Za-z0-9_]*:/ { in_block=0 }
///   in_block { print }
fn extract_kill_criteria_block(text: &str) -> String {
    let mut in_block = false;
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.starts_with("kill_criteria:") {
            in_block = true;
            continue;
        }
        if in_block && starts_with_top_level_key(line) {
            in_block = false;
        }
        if in_block {
            lines.push(line);
        }
    }
    join_lines(&lines)
}

/// `^[A-Za-z_][A-Za-z0-9_]*:` — a top-level YAML key at column 0. The awk
/// `^[A-Za-z_]...:` anchors at the start of the (untrimmed) line, so a leading
/// space (list items, nested keys) never matches.
fn starts_with_top_level_key(line: &str) -> bool {
    let bytes = line.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    let mut i = 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b':' {
            return true;
        }
        if !(b.is_ascii_alphanumeric() || b == b'_') {
            return false;
        }
        i += 1;
    }
    false
}

/// Join collected lines the way `printf '%s\n'` over awk output does: each line
/// followed by `\n`. An empty list yields an empty string (bash treats an empty
/// block as "no block").
fn join_lines(lines: &[&str]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    for l in lines {
        s.push_str(l);
        s.push('\n');
    }
    s
}

/// `_kc_parse_entries`: parse the kill_criteria YAML list into ordered entries.
/// Mirrors the awk state machine exactly:
///   - a line matching `^[[:space:]]+-[[:space:]]` starts a new entry (idx++),
///     and the `- ` marker is rewritten to two spaces so a same-line `name:` is
///     still recognized;
///   - `name:` / `predicate:` / `reason:` lines (any leading whitespace) set the
///     fields, with surrounding single/double quotes stripped.
fn parse_entries(block: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut idx = 0usize;
    let mut in_entry = false;
    let mut name = String::new();
    let mut pred = String::new();
    let mut reason = String::new();

    let flush = |entries: &mut Vec<Entry>,
                 in_entry: &mut bool,
                 idx: usize,
                 name: &mut String,
                 pred: &mut String,
                 reason: &mut String| {
        if *in_entry {
            entries.push(Entry {
                idx,
                name: std::mem::take(name),
                predicate: std::mem::take(pred),
                reason: std::mem::take(reason),
            });
            *in_entry = false;
        }
    };

    for raw in block.lines() {
        // `/^[[:space:]]+-[[:space:]]/`: at least one leading space, then `-`,
        // then a space. Starts a new entry.
        if is_list_item_start(raw) {
            flush(
                &mut entries,
                &mut in_entry,
                idx,
                &mut name,
                &mut pred,
                &mut reason,
            );
            idx += 1;
            in_entry = true;
            // `sub(/^[[:space:]]+-[[:space:]]+/, "  ", $0)`: rewrite the marker
            // to two spaces, then fall through to the field matchers on the
            // SAME (rewritten) line.
            let rewritten = rewrite_list_marker(raw);
            apply_field(&rewritten, &mut name, &mut pred, &mut reason);
            continue;
        }
        if in_entry {
            apply_field(raw, &mut name, &mut pred, &mut reason);
        }
    }
    flush(
        &mut entries,
        &mut in_entry,
        idx,
        &mut name,
        &mut pred,
        &mut reason,
    );

    entries
}

/// `/^[[:space:]]+-[[:space:]]/`: one-or-more leading whitespace, a `-`, then a
/// whitespace char.
fn is_list_item_start(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    // Need at least one leading whitespace.
    if i == 0 {
        return false;
    }
    if i >= bytes.len() || bytes[i] != b'-' {
        return false;
    }
    let after = i + 1;
    after < bytes.len() && (bytes[after] == b' ' || bytes[after] == b'\t')
}

/// `sub(/^[[:space:]]+-[[:space:]]+/, "  ", line)`: replace the leading
/// whitespace + `-` + whitespace run with exactly two spaces.
fn rewrite_list_marker(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    // i now points at `-`.
    let mut j = i + 1; // skip `-`
    while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
        j += 1;
    }
    format!("  {}", &line[j..])
}

/// Apply the `name:` / `predicate:` / `reason:` field matchers from
/// `_kc_parse_entries`. Each is `^[[:space:]]+<field>:[[:space:]]*` with the
/// value quote-stripped. The bash matches in order name, predicate, reason and
/// the `next` means at most one fires per line.
fn apply_field(line: &str, name: &mut String, pred: &mut String, reason: &mut String) {
    if let Some(v) = field_value(line, "name") {
        *name = strip_quotes(v);
    } else if let Some(v) = field_value(line, "predicate") {
        *pred = strip_quotes(v);
    } else if let Some(v) = field_value(line, "reason") {
        *reason = strip_quotes(v);
    }
}

/// Match `^[[:space:]]+<field>:[[:space:]]*` and return the remainder of the
/// line (the value). Requires at least one leading whitespace, exactly like the
/// awk `[[:space:]]+`. After `rewrite_list_marker` the inline field is prefixed
/// by two spaces so it matches here too.
fn field_value<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if i == 0 {
        return None; // needs leading whitespace
    }
    let rest = &line[i..];
    let key = format!("{field}:");
    let after = rest.strip_prefix(&key)?;
    // `[[:space:]]*`: strip following spaces/tabs only (not newlines; lines()
    // already stripped the newline).
    let trimmed = after.trim_start_matches([' ', '\t']);
    Some(trimmed)
}

/// `function strip(s) { gsub(/^["\x27]|["\x27]$/, "", s); return s }`: strip a
/// single leading AND a single trailing quote char (`"` or `'`). gsub with that
/// alternation removes one anchored match at each end.
fn strip_quotes(s: &str) -> String {
    let mut s = s.to_string();
    if let Some(first) = s.chars().next() {
        if first == '"' || first == '\'' {
            s.remove(0);
        }
    }
    if let Some(last) = s.chars().last() {
        if last == '"' || last == '\'' {
            s.pop();
        }
    }
    s
}

// ── state-file field reads (mirror `_kc_state_field` / `_kc_consecutive_failures`)

/// `_kc_state_field`: read a top-level `field:` from the state file.
///   sed -n "s/^[[:space:]]*${field}:[[:space:]]*//p" | head -1 | sed 's/ *$//' | tr -d '"'
/// Returns None when the file is unreadable; Some("") when the key is absent
/// (the bash pipeline emits an empty string in that case).
fn state_field(state_file: &Path, field: &str) -> Option<String> {
    let content = std::fs::read_to_string(state_file).ok()?;
    for line in content.lines() {
        // `^[[:space:]]*${field}:` — optional leading whitespace, then key.
        let trimmed = line.trim_start_matches([' ', '\t']);
        let key = format!("{field}:");
        if let Some(after) = trimmed.strip_prefix(&key) {
            // `[[:space:]]*` after the colon, then trailing-space strip + quote
            // removal.
            let val = after.trim_start_matches([' ', '\t']);
            let val = val.trim_end_matches([' ', '\t']);
            let val = val.replace('"', ""); // tr -d '"'
            return Some(val);
        }
    }
    Some(String::new())
}

/// `_kc_consecutive_failures`: extract `consecutive_failures:` from the nested
/// `verification:` block. Mirrors the awk:
///   /^verification:/ { in_block=1; next }
///   in_block && /^[A-Za-z_]/ { in_block=0 }
///   in_block && /^[[:space:]]+consecutive_failures:/ { ...print; exit }
/// Returns "0" when the file is unreadable or the key is absent.
fn consecutive_failures(state_file: &Path) -> String {
    let Ok(content) = std::fs::read_to_string(state_file) else {
        return "0".to_string();
    };
    let mut in_block = false;
    for line in content.lines() {
        if line.starts_with("verification:") {
            in_block = true;
            continue;
        }
        // `in_block && /^[A-Za-z_]/`: a new top-level key (column 0 letter/_)
        // ends the block. This is evaluated BEFORE the inner match, exactly
        // like the awk rule order.
        if in_block {
            if let Some(&first) = line.as_bytes().first() {
                if first.is_ascii_alphabetic() || first == b'_' {
                    in_block = false;
                }
            }
        }
        if in_block {
            // `^[[:space:]]+consecutive_failures:` — leading whitespace required.
            let bytes = line.as_bytes();
            let mut i = 0;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            if i > 0 {
                let rest = &line[i..];
                if let Some(after) = rest.strip_prefix("consecutive_failures:") {
                    let v = after
                        .trim_start_matches([' ', '\t'])
                        .trim_end_matches([' ', '\t']);
                    return v.to_string();
                }
            }
        }
    }
    "0".to_string()
}

/// `_kc_is_int`: `^[0-9]+$`.
fn is_int(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

// ── predicate evaluators (mirror `_kc_eval_*`) ────────────────────────────────

/// Evaluator result: Fired (0), NotFired (1), Malformed (2). Mirrors the bash
/// return codes used by `_kc_dispatch_predicate`.
#[derive(Debug, PartialEq, Eq)]
enum Eval {
    Fired,
    NotFired,
    Malformed,
}

/// `_kc_eval_iteration`: `^iteration[[:space:]]*(>|>=)[[:space:]]*([0-9]+)[[:space:]]*$`.
/// Reads `iteration` from the state file; non-int defaults to 1 (bash
/// `_kc_is_int "$cur" || cur=1`).
fn eval_iteration(pred: &str, state_file: &Path) -> Eval {
    let Some((op, rhs)) = parse_cmp(pred, "iteration") else {
        return Eval::Malformed;
    };
    let cur_raw = state_field(state_file, "iteration").unwrap_or_default();
    let cur: i64 = if is_int(&cur_raw) {
        cur_raw.parse().unwrap_or(1)
    } else {
        1
    };
    cmp_fires(cur, op, rhs)
}

/// `_kc_eval_stuck_test`:
/// `^same_test_failing_for[[:space:]]*(>|>=)[[:space:]]*([0-9]+)[[:space:]]*$`.
/// Reads consecutive_failures; non-int defaults to 0.
fn eval_stuck_test(pred: &str, state_file: &Path) -> Eval {
    let Some((op, rhs)) = parse_cmp(pred, "same_test_failing_for") else {
        return Eval::Malformed;
    };
    let failures_raw = consecutive_failures(state_file);
    let failures: i64 = if is_int(&failures_raw) {
        failures_raw.parse().unwrap_or(0)
    } else {
        0
    };
    cmp_fires(failures, op, rhs)
}

/// Parse `^<key>[[:space:]]*(>|>=)[[:space:]]*([0-9]+)[[:space:]]*$` and return
/// (op, rhs). Op is ">" or ">=".
fn parse_cmp(pred: &str, key: &str) -> Option<(&'static str, i64)> {
    let rest = pred.strip_prefix(key)?;
    let rest = rest.trim_start_matches([' ', '\t']);
    // Operator: ">=" must be tried before ">".
    let (op, after): (&'static str, &str) = if let Some(a) = rest.strip_prefix(">=") {
        (">=", a)
    } else if let Some(a) = rest.strip_prefix('>') {
        (">", a)
    } else {
        return None;
    };
    let after = after.trim_start_matches([' ', '\t']);
    // RHS must be `[0-9]+` then optional trailing whitespace to end-of-string.
    let digits_end = after
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after.len());
    if digits_end == 0 {
        return None; // no digits
    }
    let digits = &after[..digits_end];
    let tail = &after[digits_end..];
    if !tail.chars().all(|c| c == ' ' || c == '\t') {
        return None; // trailing non-whitespace -> not anchored `$`
    }
    let rhs: i64 = digits.parse().ok()?;
    Some((op, rhs))
}

/// Apply `>` / `>=` and return Fired/NotFired.
fn cmp_fires(lhs: i64, op: &str, rhs: i64) -> Eval {
    let fired = match op {
        ">" => lhs > rhs,
        ">=" => lhs >= rhs,
        _ => false,
    };
    if fired {
        Eval::Fired
    } else {
        Eval::NotFired
    }
}

/// `_kc_eval_files_outside`:
/// `^files_outside\(plan_path\)[[:space:]]*>[[:space:]]*([0-9]+)[[:space:]]*$`.
/// Counts files in the git diff (branch-vs-baseline + unstaged + staged) that
/// are OUTSIDE the plan folder. See the bash for the baseline-resolution order.
fn eval_files_outside(pred: &str, plan_path: &str, git_bin: &str) -> Eval {
    // Only `>` is supported (no `>=` form in the bash).
    let rest = match pred.strip_prefix("files_outside(plan_path)") {
        Some(r) => r.trim_start_matches([' ', '\t']),
        None => return Eval::Malformed,
    };
    let after = match rest.strip_prefix('>') {
        Some(a) => a.trim_start_matches([' ', '\t']),
        None => return Eval::Malformed,
    };
    let digits_end = after
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after.len());
    if digits_end == 0 {
        return Eval::Malformed;
    }
    let tail = &after[digits_end..];
    if !tail.chars().all(|c| c == ' ' || c == '\t') {
        return Eval::Malformed;
    }
    let rhs: i64 = match after[..digits_end].parse() {
        Ok(v) => v,
        Err(_) => return Eval::Malformed,
    };

    // `root=$(git rev-parse --show-toplevel)`; empty -> return 1 (not fired).
    let root = match git_show_toplevel(git_bin) {
        Some(r) => r,
        None => return Eval::NotFired,
    };

    // Baseline resolution: @{u} merge-base, then origin/main|master|HEAD.
    let base = resolve_baseline(git_bin, &root);

    // diff_list = (branch-vs-base if base) + unstaged + staged, then `awk NF |
    // sort -u`.
    let mut raw = String::new();
    if let Some(b) = &base {
        raw.push_str(&git_diff_name_only(
            git_bin,
            &root,
            &["diff", "--name-only", b, "HEAD"],
        ));
    }
    raw.push_str(&git_diff_name_only(
        git_bin,
        &root,
        &["diff", "--name-only"],
    ));
    raw.push_str(&git_diff_name_only(
        git_bin,
        &root,
        &["diff", "--name-only", "--cached"],
    ));

    let diff_list = sort_unique_nonempty(&raw);

    // Count files OUTSIDE plan_path. If plan_path is inside root, skip files
    // under it (exact match OR `rel/` prefix). Otherwise every file counts.
    //
    // Canonicalize plan_path before the prefix check (gemini PR #515 high):
    // `root` comes from `git rev-parse --show-toplevel`, which is always
    // absolute and symlink-resolved. A relative or non-canonical plan_path
    // (e.g. `plan`, `./plan`, or a macOS `/var` tempdir that resolves to
    // `/private/var`) would strip_prefix to None, miscounting in-plan files as
    // OUTSIDE and firing falsely. canonicalize() needs the path to exist; on
    // failure fall back to the raw value (which then never matches an absolute
    // root - the pre-fix behavior for a nonexistent plan_path).
    let plan_path_abs = std::fs::canonicalize(plan_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| plan_path.to_string());
    let root_str = root.to_string_lossy();
    let rel_inside: Option<String> = {
        let prefix = format!("{root_str}/");
        plan_path_abs.strip_prefix(&prefix).map(|s| s.to_string())
    };

    let mut count: i64 = 0;
    for f in &diff_list {
        if f.is_empty() {
            continue;
        }
        if let Some(rel) = &rel_inside {
            if f == rel {
                continue;
            }
            if f.starts_with(&format!("{rel}/")) {
                continue;
            }
        }
        count += 1;
    }

    if count > rhs {
        Eval::Fired
    } else {
        Eval::NotFired
    }
}

/// `_kc_eval_test_file_deleted`: `^any_test_file_deleted[[:space:]]*$`.
/// Fires when `git status --porcelain` shows a deletion or rename-from of a
/// test-file path.
fn eval_test_file_deleted(pred: &str, git_bin: &str) -> Eval {
    // `^any_test_file_deleted[[:space:]]*$`.
    let rest = match pred.strip_prefix("any_test_file_deleted") {
        Some(r) => r,
        None => return Eval::Malformed,
    };
    if !rest.chars().all(|c| c == ' ' || c == '\t') {
        return Eval::Malformed;
    }

    let root = match git_show_toplevel(git_bin) {
        Some(r) => r,
        None => return Eval::NotFired,
    };

    let status = git_status_porcelain(git_bin, &root);
    for line in status.lines() {
        // Mirror the bash awk extraction of candidate paths:
        //   /^[[:space:]]*D /{print $2}  /^D[[:space:]]/{print $2}
        //   /^[[:space:]]*R /{print $2}
        // awk default field-splitting on whitespace: $2 is the second
        // whitespace-delimited token. For a porcelain line like ` D path`, $1=D
        // $2=path; for `R  old -> new`, $1=R $2=old.
        if let Some(path) = porcelain_candidate_path(line) {
            if is_test_path(&path) {
                return Eval::Fired;
            }
        }
    }
    Eval::NotFired
}

/// Extract the awk `$2` candidate path for the three porcelain patterns the
/// bash matches: `^[[:space:]]*D `, `^D[[:space:]]`, `^[[:space:]]*R `.
fn porcelain_candidate_path(line: &str) -> Option<String> {
    // Match any of the three anchored patterns (replicating the awk addresses).
    let matches_pattern = {
        let lead_trimmed = line.trim_start_matches([' ', '\t']);
        // `^[[:space:]]*D ` (leading ws then `D `) OR `^D[[:space:]]` (literal
        // D at col 0 then whitespace) OR `^[[:space:]]*R ` (leading ws then `R `).
        let d_space = lead_trimmed.starts_with("D ");
        let r_space = lead_trimmed.starts_with("R ");
        // `^D[[:space:]]`: first char D and second char whitespace (no leading
        // ws). Porcelain `D ` (deleted-in-index) hits this; covered by d_space
        // too, but kept for fidelity to the separate awk address.
        let d_at_zero = {
            let b = line.as_bytes();
            b.first() == Some(&b'D') && b.get(1).map(|&c| c == b' ' || c == b'\t').unwrap_or(false)
        };
        d_space || r_space || d_at_zero
    };
    if !matches_pattern {
        return None;
    }
    // awk default split: collapse whitespace, take the 2nd field.
    let fields: Vec<&str> = line.split_whitespace().collect();
    fields.get(1).map(|s| s.to_string())
}

/// `grep -E '(^|/)(__tests__/|tests?/|spec/|.*\.(test|spec)\.(ts|tsx|js|jsx|py|sh)$|test_.*\.(py|sh)$|.*_test\.(py|go)$)'`.
/// A path that matches any of the test alternatives (anchored at start or after
/// a `/`) is a test path.
fn is_test_path(path: &str) -> bool {
    // The grep is unanchored (a match anywhere in the line). Each candidate
    // path is a single token. We replicate the alternation:
    //   (^|/)__tests__/
    //   (^|/)tests?/
    //   (^|/)spec/
    //   (^|/).*\.(test|spec)\.(ts|tsx|js|jsx|py|sh)$
    //   (^|/)test_.*\.(py|sh)$
    //   (^|/).*_test\.(py|go)$
    // The `(^|/)` prefix means each segment-boundary is a valid start. For the
    // directory forms we check segment membership; for the filename forms we
    // check the basename suffix (the `(^|/).*` consumes any prefix).

    // Directory-segment forms: a path component equal to __tests__, tests, test,
    // or spec, honoring the trailing `/` in the regex - the segment must be
    // followed by `/` in the original path (i.e. not the final component), which
    // is exactly what path_has_dir_segment checks.
    if path_has_dir_segment(path, "__tests__")
        || path_has_dir_segment(path, "tests")
        || path_has_dir_segment(path, "test")
        || path_has_dir_segment(path, "spec")
    {
        return true;
    }

    // Filename-suffix forms operate on the basename.
    let base = path.rsplit('/').next().unwrap_or(path);

    // `.*\.(test|spec)\.(ts|tsx|js|jsx|py|sh)$`
    if has_double_ext(
        base,
        &["test", "spec"],
        &["ts", "tsx", "js", "jsx", "py", "sh"],
    ) {
        return true;
    }
    // `test_.*\.(py|sh)$` — but the `(^|/)` anchor means the basename (or the
    // part after a `/`) starts with `test_`. Since base is the last component,
    // require base starts with `test_` and ends with .py/.sh.
    if base.starts_with("test_") && (base.ends_with(".py") || base.ends_with(".sh")) {
        return true;
    }
    // `.*_test\.(py|go)$`
    if (base.ends_with(".py") || base.ends_with(".go")) && {
        let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base);
        stem.ends_with("_test")
    } {
        return true;
    }

    false
}

/// True iff `seg` appears as a directory segment in `path` (i.e. `seg` is a
/// path component that is followed by a `/`, honoring the regex `(^|/)seg/`).
fn path_has_dir_segment(path: &str, seg: &str) -> bool {
    // `(^|/)<seg>/` — <seg> preceded by start-or-slash and followed by slash.
    // Scan for `seg/` occurrences and check the preceding boundary.
    let needle = format!("{seg}/");
    let mut start = 0;
    while let Some(pos) = path[start..].find(&needle) {
        let abs = start + pos;
        let preceded_ok = abs == 0 || path.as_bytes()[abs - 1] == b'/';
        if preceded_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// `.*\.(a|b)\.(x|y)$` — basename ends with `.<mid>.<ext>` where mid ∈ mids and
/// ext ∈ exts.
fn has_double_ext(base: &str, mids: &[&str], exts: &[&str]) -> bool {
    // Split into stem.mid.ext from the right.
    let Some((rest, ext)) = base.rsplit_once('.') else {
        return false;
    };
    if !exts.contains(&ext) {
        return false;
    }
    let Some((_, mid)) = rest.rsplit_once('.') else {
        return false;
    };
    mids.contains(&mid)
}

// ── git shellouts (test-overridable via --git-bin / FNO_KILLCHECK_GIT_BIN) ─────

fn git_show_toplevel(git_bin: &str) -> Option<PathBuf> {
    let out = Command::new(git_bin)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// Resolve the diff baseline: `@{u}` merge-base, then origin/main|master|HEAD.
fn resolve_baseline(git_bin: &str, root: &Path) -> Option<String> {
    let upstream = Command::new(git_bin)
        .current_dir(root)
        .args(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(u) = upstream {
        if let Some(base) = git_merge_base(git_bin, root, "HEAD", &u) {
            return Some(base);
        }
    }
    for candidate in ["origin/main", "origin/master", "origin/HEAD"] {
        if let Some(base) = git_merge_base(git_bin, root, "HEAD", candidate) {
            if !base.is_empty() {
                return Some(base);
            }
        }
    }
    None
}

fn git_merge_base(git_bin: &str, root: &Path, a: &str, b: &str) -> Option<String> {
    let out = Command::new(git_bin)
        .current_dir(root)
        .args(["merge-base", a, b])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn git_diff_name_only(git_bin: &str, root: &Path, args: &[&str]) -> String {
    let out = Command::new(git_bin).current_dir(root).args(args).output();
    match out {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            if !s.ends_with('\n') && !s.is_empty() {
                s.push('\n');
            }
            s
        }
        Err(_) => String::new(),
    }
}

fn git_status_porcelain(git_bin: &str, root: &Path) -> String {
    let out = Command::new(git_bin)
        .current_dir(root)
        .args(["status", "--porcelain"])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

/// `awk 'NF' | sort -u`: drop blank lines, sort, deduplicate.
fn sort_unique_nonempty(raw: &str) -> Vec<String> {
    let mut v: Vec<String> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect();
    v.sort();
    v.dedup();
    v
}

// ── dispatch + main entry (mirror `_kc_dispatch_predicate` / `check_kill_criteria`)

/// `_kc_dispatch_predicate`: route a predicate string to its evaluator.
/// The bash `case` matches on a PREFIX (`iteration*`, `same_test_failing_for*`,
/// `files_outside(plan_path)*`, `any_test_file_deleted*`); the evaluator then
/// re-validates the full string and may return Malformed.
fn dispatch_predicate(pred: &str, plan_path: &str, state_file: &Path, git_bin: &str) -> Eval {
    // The bash case patterns use `iteration[[:space:]]*` etc., i.e. the keyword
    // optionally followed by whitespace. A predicate that merely STARTS with the
    // keyword routes to that evaluator; the evaluator's own regex decides
    // malformed-ness. `files_outside(plan_path)*` and `any_test_file_deleted*`
    // are prefix globs.
    if pred.starts_with("iteration") {
        eval_iteration(pred, state_file)
    } else if pred.starts_with("same_test_failing_for") {
        eval_stuck_test(pred, state_file)
    } else if pred.starts_with("files_outside(plan_path)") {
        eval_files_outside(pred, plan_path, git_bin)
    } else if pred.starts_with("any_test_file_deleted") {
        eval_test_file_deleted(pred, git_bin)
    } else {
        Eval::Malformed
    }
}

/// Core logic shared by the print + capture entries.
fn run(args: &[String]) -> RunOutput {
    let mut out = RunOutput {
        code: 0,
        stdout: String::new(),
        stderr: String::new(),
    };
    let parsed = parse_args(args);

    // `check_kill_criteria`: empty plan_path -> WARN + return 0.
    let plan_path = match &parsed.plan_path {
        Some(p) if !p.is_empty() => p.clone(),
        _ => {
            log_warn(
                &mut out,
                &parsed,
                "check_kill_criteria called without plan_path - skipping",
            );
            return out;
        }
    };

    // `block=$(_kc_read_plan_block ...) || return 0`; empty block -> return 0.
    let block = match read_plan_block(&plan_path) {
        Some(b) if !b.is_empty() => b,
        _ => return out,
    };

    let entries = parse_entries(&block);
    if entries.is_empty() {
        return out;
    }

    let state_file = resolve_field_state_file(&parsed);

    for ent in &entries {
        // `[[ -z "$ent_name" ]] || [[ -z "$ent_pred" ]]` -> WARN + skip.
        if ent.name.is_empty() || ent.predicate.is_empty() {
            log_warn(
                &mut out,
                &parsed,
                &format!(
                    "kill_criteria entry {} missing required fields - skipping",
                    ent.idx
                ),
            );
            continue;
        }
        match dispatch_predicate(&ent.predicate, &plan_path, &state_file, &parsed.git_bin) {
            Eval::Fired => {
                // `printf 'KILL_CRITERIA_FIRED %s|%s\n' "$ent_name" "${ent_reason:-$ent_name}"`
                let reason = if ent.reason.is_empty() {
                    ent.name.clone()
                } else {
                    ent.reason.clone()
                };
                out.stdout
                    .push_str(&format!("KILL_CRITERIA_FIRED {}|{}\n", ent.name, reason));
                out.code = 1;
                return out;
            }
            Eval::Malformed => {
                log_warn(
                    &mut out,
                    &parsed,
                    &format!(
                        "kill_criteria entry '{}' predicate '{}' is unparseable - skipping",
                        ent.name, ent.predicate
                    ),
                );
            }
            Eval::NotFired => {}
        }
    }

    out
}

// ── public entry points (mirror loopcheck's run_X / run_X_capture split) ──────

/// Print stdout/stderr and return the exit code. Used by `bin/client.rs`.
pub fn run_kill_check(args: &[String]) -> i32 {
    let out = run(args);
    if !out.stdout.is_empty() {
        print!("{}", out.stdout);
    }
    if !out.stderr.is_empty() {
        eprint!("{}", out.stderr);
    }
    out.code
}

/// Test-friendly variant: returns (exit_code, stdout, stderr) without printing.
pub fn run_kill_check_capture(args: &[String]) -> (i32, String, String) {
    let out = run(args);
    (out.code, out.stdout, out.stderr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cmp_iteration_forms() {
        assert_eq!(parse_cmp("iteration > 3", "iteration"), Some((">", 3)));
        assert_eq!(parse_cmp("iteration>=10", "iteration"), Some((">=", 10)));
        assert_eq!(parse_cmp("iteration  >  5  ", "iteration"), Some((">", 5)));
        assert_eq!(parse_cmp("iteration < 3", "iteration"), None);
        assert_eq!(parse_cmp("iteration > x", "iteration"), None);
    }

    #[test]
    fn strip_quotes_one_each_end() {
        assert_eq!(strip_quotes("\"hello\""), "hello");
        assert_eq!(strip_quotes("'hi'"), "hi");
        assert_eq!(strip_quotes("plain"), "plain");
        assert_eq!(strip_quotes("\"only-left"), "only-left");
    }

    #[test]
    fn top_level_key_anchoring() {
        assert!(starts_with_top_level_key("waves:"));
        assert!(starts_with_top_level_key("kill_criteria:"));
        assert!(!starts_with_top_level_key("  - name: foo"));
        assert!(!starts_with_top_level_key("  predicate: x"));
        assert!(!starts_with_top_level_key("123abc:"));
    }

    #[test]
    fn parse_entries_inline_and_block() {
        let block = "  - name: iter\n    predicate: iteration > 3\n    reason: too many\n  - name: stuck\n    predicate: same_test_failing_for >= 5\n";
        let entries = parse_entries(block);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "iter");
        assert_eq!(entries[0].predicate, "iteration > 3");
        assert_eq!(entries[0].reason, "too many");
        assert_eq!(entries[1].name, "stuck");
        assert_eq!(entries[1].predicate, "same_test_failing_for >= 5");
        assert_eq!(entries[1].reason, "");
    }

    #[test]
    fn test_path_classification() {
        assert!(is_test_path("src/__tests__/foo.js"));
        assert!(is_test_path("tests/test_x.py"));
        assert!(is_test_path("a/b/spec/thing.rb"));
        assert!(is_test_path("foo.test.ts"));
        assert!(is_test_path("bar.spec.jsx"));
        assert!(is_test_path("test_helper.py"));
        assert!(is_test_path("widget_test.go"));
        assert!(is_test_path("module_test.py"));
        assert!(!is_test_path("src/main.rs"));
        assert!(!is_test_path("README.md"));
        // bare last-segment `tests` (no trailing slash) should NOT match the
        // directory form (regex requires a trailing `/`).
        assert!(!is_test_path("path/to/tests"));
    }

    #[test]
    fn double_ext_matcher() {
        assert!(has_double_ext(
            "foo.test.ts",
            &["test", "spec"],
            &["ts", "tsx", "js", "jsx", "py", "sh"]
        ));
        assert!(!has_double_ext(
            "foo.test.rs",
            &["test", "spec"],
            &["ts", "tsx", "js", "jsx", "py", "sh"]
        ));
        assert!(!has_double_ext(
            "foo.ts",
            &["test", "spec"],
            &["ts", "tsx", "js", "jsx", "py", "sh"]
        ));
    }

    #[test]
    fn porcelain_path_extraction() {
        assert_eq!(
            porcelain_candidate_path(" D tests/test_x.py").as_deref(),
            Some("tests/test_x.py")
        );
        assert_eq!(
            porcelain_candidate_path("D  tests/test_x.py").as_deref(),
            Some("tests/test_x.py")
        );
        assert_eq!(
            porcelain_candidate_path("R  old_test.py -> new.py").as_deref(),
            Some("old_test.py")
        );
        assert_eq!(porcelain_candidate_path(" M src/main.rs"), None);
    }
}
