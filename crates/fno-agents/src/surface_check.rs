//! `surface-check`: the plan `surface:` block gate (scripts/validate-plan.sh
//! step 2b-bis gate), ported from the Python heredoc that used to live in
//! `scripts/validate-plan.sh`, plus the cross-language symbol walk the heredoc
//! never had.
//!
//! Law d-b6cc1a2a moves behavior between `cli/src/fno` and the `crates/` tree,
//! so a sweep that names one tree's paths misses the twin in the other. The
//! walk greps each changed answerer's symbol at the merge-base tree and
//! refuses a reader in another language tree that no answerer names.
//!
//! Direct dispatch, daemon-free: `scripts/validate-plan.sh` shells HERE and
//! reads the tab-separated line protocol. Exit 0 always; the lines carry the
//! verdict:
//!
//! ```text
//! E  error
//! W  warning the caller graduates (quick / pre-gate date)
//! X  refusal the caller graduates (quick / pre-gate date)
//! O  receipt
//! U  the block could not be checked
//! ```
//!
//! The shape half is a line-for-line port of the old `surface_prog` heredoc,
//! messages byte-for-byte (the AC10 tests grep them); values render the way
//! Python `%s` renders them (`None`, `True`, raw strings) so a message quoted
//! in a test or review survives the port unchanged.

use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

const USAGE: &str = "usage: fno-agents surface-check <plan.md> [--repo <dir>] [--base <ref>]\n";

const DISPOSITIONS: [&str; 4] = [
    "dual-logic",
    "shared-vocabulary",
    "generated-artifact",
    "out-of-scope",
];
const CHANGED: [&str; 2] = ["dual-logic", "shared-vocabulary"];

// ponytail: cap sized by the 2026-09-14 replay (362 post-gate blocks: p90 of
// unlisted files falls 9 -> 7 at 10; `load_registry` is 75 files). Widen only
// with a fresh replay in hand.
const WIDE_SYMBOL_FILES: usize = 10;

/// A walk hit's language tree, from the file extension.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Lang {
    Python,
    Rust,
    Shell,
    Ts,
    Go,
}

impl Lang {
    fn of_path(path: &str) -> Option<Lang> {
        let ext = path.rsplit('.').next()?;
        match ext {
            "py" => Some(Lang::Python),
            "rs" => Some(Lang::Rust),
            "sh" | "bash" => Some(Lang::Shell),
            "ts" | "tsx" | "js" | "mjs" => Some(Lang::Ts),
            "go" => Some(Lang::Go),
            _ => None,
        }
    }

    /// The name the X/W lines and the walk receipt print.
    fn name(self) -> &'static str {
        match self {
            Lang::Python => "python",
            Lang::Rust => "rust",
            Lang::Shell => "shell",
            Lang::Ts => "ts",
            Lang::Go => "go",
        }
    }
}

/// One `answerers:` entry the walk needs. Shape messages read the serde Value
/// directly; only the walk builds this.
struct WalkAnswerer {
    /// The `at:` value as written; the no-symbol warning quotes it.
    at_text: String,
    /// First `at:` token with any trailing `:<lines>` stripped - the coverage
    /// path. Match on the full path, never the basename (`cli.py`, `lib.rs`
    /// and `mod.rs` collide).
    path: String,
    lang: Option<Lang>,
    disposition: String,
    reads: String,
}

/// `fno-agents surface-check <plan.md> [--repo <dir>] [--base <ref>]`.
/// Exit 0 always; usage errors exit 2 so the shell caller warns NOT CHECKED
/// instead of reading a misuse as a verdict.
pub fn run_surface_check(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!(
            "{USAGE}\n\
             Checks a plan's surface: block and walks each changed answerer's\n\
             symbol into every language tree at the merge base.\n\n\
             Line protocol (stdout, TAB-separated, one per line):\n\
             \x20 E\t<error>     the block fails the gate\n\
             \x20 W\t<warning>   walk finding the caller graduates\n\
             \x20 X\t<refusal>   a reader in another tree no answerer names\n\
             \x20 O\t<receipt>   progress; printed beside a clean block\n\
             \x20 U\t<reason>    the block could not be checked\n\
             Exit 0 always; the lines carry the verdict.\n"
        );
        return 0;
    }

    let mut rest: &[String] = args;
    // Tolerate a leading verb token (kill-check does the same).
    if rest.first().map(String::as_str) == Some("surface-check") {
        rest = &rest[1..];
    }
    let mut plan: Option<String> = None;
    let mut repo: Option<String> = None;
    let mut base: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        let arg = &rest[i];
        let take_value = |name: &str| -> Option<String> {
            if let Some(v) = arg.strip_prefix(&format!("{name}=")) {
                Some(v.to_string())
            } else if *arg == *name {
                rest.get(i + 1).cloned()
            } else {
                None
            }
        };
        if let Some(v) = take_value("--repo") {
            repo = Some(v);
            if arg == "--repo" {
                i += 1;
            }
        } else if let Some(v) = take_value("--base") {
            base = Some(v);
            if arg == "--base" {
                i += 1;
            }
        } else if arg.starts_with('-') {
            eprint!("{USAGE}");
            return 2;
        } else if plan.is_none() {
            plan = Some(arg.clone());
        } else {
            eprint!("{USAGE}");
            return 2;
        }
        i += 1;
    }
    let Some(plan) = plan else {
        eprint!("{USAGE}");
        return 2;
    };

    let mut out = String::new();
    let clean = shape_check(&plan, &mut out);
    if clean {
        // A plan's own new files are not answerers that existed before it, so
        // the walk reads the merge-base tree, not the worktree.
        cross_language_walk(&plan, repo.as_deref(), base.as_deref(), &mut out);
    }
    print!("{out}");
    0
}

// ── shape check: the ported surface_prog heredoc ─────────────────────────────

/// Runs the shape check, appending `E`/`O`/`U` lines to `out`. Returns whether
/// the block is clean (no `E`, no `U`) - only a clean block earns the walk.
fn shape_check(plan_path: &str, out: &mut String) -> bool {
    let text = match std::fs::read_to_string(plan_path) {
        Ok(t) => t,
        Err(_) => {
            // The heredoc crashed here and the shell caller warned NOT CHECKED;
            // the U line is that, gracefully.
            out.push_str("U\tplan file could not be read\n");
            return false;
        }
    };
    let Some(fm) = frontmatter(&text) else {
        out.push_str("U\tno closed --- frontmatter block\n");
        return false;
    };
    let loaded: Value = match serde_yaml_ng::from_str(&fm) {
        Ok(v) => v,
        Err(exc) => {
            let norm: String = format!("{exc}")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let clipped: String = norm.chars().take(160).collect();
            out.push_str(&format!("U\t{clipped}\n"));
            return false;
        }
    };
    // `(loaded or {}).get("surface")`: a null document is an empty mapping. A
    // non-mapping document crashes the Python (the shell warns NOT CHECKED);
    // the U line here is that crash, gracefully.
    if !loaded.is_null() && !loaded.is_object() {
        out.push_str("U\tfrontmatter is not a YAML mapping\n");
        return false;
    }
    let surface = loaded.get("surface");
    let Some(block) = surface.and_then(Value::as_object) else {
        out.push_str(&format!(
            "E\tsurface: must be a block of keys (question, sweep, answerers, count, \
             count_after), not `{}`\n",
            py_str(surface.unwrap_or(&Value::Null))
        ));
        return false;
    };

    let question = block.get("question");
    let Some(question_text) = question
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty())
    else {
        out.push_str("E\tsurface.question is empty - phrase the question in one line\n");
        return false;
    };
    if !question_text.ends_with('?') {
        out.push_str(&format!(
            "E\tsurface.question `{question_text}` does not end in a question mark - \
             phrase the unit as a question, never a noun phrase\n"
        ));
        // The heredoc does not stop here; the sweep tooth still runs.
    }

    if !block.get("sweep").map(nonempty_str).unwrap_or(false) {
        out.push_str(
            "E\tsurface.sweep is empty - an empty sweep is the I-never-looked case this \
             field exists to distinguish from I-looked-and-found-one\n",
        );
        return false;
    }

    let answerers = match block.get("answerers").and_then(Value::as_array) {
        Some(a) if !a.is_empty() => a,
        _ => {
            out.push_str(
                "E\tsurface.answerers is empty or missing - a question with zero answerers \
                 is not answered by this repo, and the block exists to prove the looking\n",
            );
            return false;
        }
    };

    // First pass: every answerer carries a disposition (and a reason when
    // out-of-scope).
    let mut undisposed: Vec<String> = Vec::new();
    for (idx, entry) in answerers.iter().enumerate() {
        let idx = idx + 1;
        let Some(entry) = entry.as_object() else {
            undisposed.push(format!("<answerer {idx}: not a mapping>"));
            continue;
        };
        let at_text = match entry.get("at").filter(|a| nonempty_str(a)) {
            Some(at) => at.as_str().unwrap().trim().to_string(),
            None => format!("<answerer {idx}>"),
        };
        let disposition = entry.get("disposition");
        let known = disposition
            .and_then(Value::as_str)
            .map(|d| DISPOSITIONS.contains(&d))
            .unwrap_or(false);
        if !known {
            undisposed.push(format!(
                "{at_text} (disposition `{}` is not one of {})",
                py_str(disposition.unwrap_or(&Value::Null)),
                DISPOSITIONS.join(" | ")
            ));
            continue;
        }
        if disposition.and_then(Value::as_str) == Some("out-of-scope")
            && !entry.get("reason").map(nonempty_str).unwrap_or(false)
        {
            undisposed.push(format!("{at_text} (out-of-scope with no reason)"));
        }
    }

    // Second pass: a changed answerer names its measured feed.
    for (idx, entry) in answerers.iter().enumerate() {
        let idx = idx + 1;
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let Some(at) = entry.get("at").filter(|a| nonempty_str(a)) else {
            out.push_str(&format!(
                "E\tsurface.answerers entry {idx} has no `at:` - name the site so a \
                 reviewer can open it\n"
            ));
            continue;
        };
        let changed = entry
            .get("disposition")
            .and_then(Value::as_str)
            .map(|d| CHANGED.contains(&d))
            .unwrap_or(false);
        let feeds = entry.get("reads").map(nonempty_str).unwrap_or(false)
            && entry.get("emits").map(nonempty_str).unwrap_or(false);
        if changed && !feeds {
            out.push_str(&format!(
                "E\tPlan changes {} but names no feed for it. Quote the expression it \
                 evaluates, with its line, and what that feed emits, measured.\n",
                at.as_str().unwrap().trim()
            ));
        }
    }

    if !undisposed.is_empty() {
        out.push_str(&format!(
            "E\tQuestion {question_text} has {n} answerers; the plan disposes of {kept}. \
             Every answerer is dual-logic, shared-vocabulary, generated-artifact, or \
             out-of-scope with a reason. Undisposed: {list}.\n",
            n = answerers.len(),
            kept = answerers.len() - undisposed.len(),
            list = undisposed.join("; "),
        ));
    }

    let count = block.get("count");
    let Some(count) = count.filter(|c| is_int(c)) else {
        out.push_str(&format!(
            "E\tsurface.count is `{}` - an integer stating the PR estimate is the whole \
             point of the block\n",
            py_str(count.unwrap_or(&Value::Null))
        ));
        return false;
    };
    let count = count.as_i64().unwrap_or(0);
    if count != answerers.len() as i64 {
        out.push_str(&format!(
            "E\tsurface.count is {count} but the block lists {n} answerer(s) - the count \
             is the estimate and must match what was found\n",
            n = answerers.len(),
        ));
        return false;
    }

    let count_after = block.get("count_after");
    let Some(count_after) = count_after.filter(|c| is_int(c)) else {
        out.push_str(&format!(
            "E\tsurface.count_after is `{}` - state in a number how many answerers survive \
             this plan, even when the number equals count\n",
            py_str(count_after.unwrap_or(&Value::Null))
        ));
        return false;
    };
    let count_after = count_after.as_i64().unwrap_or(0);
    if count_after > count {
        out.push_str(&format!(
            "E\tsurface.count_after ({count_after}) exceeds count ({count}) - a plan cannot \
             leave more answerers than it found\n"
        ));
        return false;
    }

    if let Some(control) = block.get("control").filter(|c| nonempty_str(c)) {
        let control = control.as_str().unwrap().trim();
        let ats: Vec<&str> = answerers
            .iter()
            .filter_map(|e| e.get("at"))
            .filter(|a| nonempty_str(a))
            .map(|a| a.as_str().unwrap().trim())
            .collect();
        let heads: Vec<&str> = ats.iter().map(|a| at_head(a)).collect();
        if heads.iter().any(|h| *h == at_head(control)) {
            out.push_str(&format!("O\tcontrol `{control}` returned by the sweep\n"));
        } else {
            let listed = heads
                .iter()
                .map(|h| format!("`{h}`"))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!(
                "E\tsurface.control `{control}` names no listed answerer (listed: {listed}); \
                 a control matches an answerer's path and symbol, and the note is ignored\n"
            ));
        }
    }
    out.push_str(&format!(
        "O\tquestion `{question_text}`, {n} answerer(s), count {count}, count_after \
         {count_after}\n",
        n = answerers.len(),
    ));
    true
}

// ── cross-language walk ──────────────────────────────────────────────────────

/// Walks each changed answerer's symbol into the other language trees at the
/// merge-base tree, appending `X`/`W`/`O` lines to `out`.
fn cross_language_walk(plan_path: &str, repo: Option<&str>, base: Option<&str>, out: &mut String) {
    let Ok(content) = std::fs::read_to_string(plan_path) else {
        return; // the shape check already reported the unreadable plan
    };
    let Some(fm) = frontmatter(&content) else {
        return;
    };
    let Ok(loaded) = serde_yaml_ng::from_str::<Value>(&fm) else {
        return; // shape check already reported the parse failure
    };
    let Some(answerers) = loaded
        .get("surface")
        .and_then(|s| s.get("answerers"))
        .and_then(Value::as_array)
    else {
        return;
    };
    let walked: Vec<WalkAnswerer> = answerers
        .iter()
        .filter_map(|e| {
            let at = e.get("at")?.as_str()?;
            let first = at.split_whitespace().next()?;
            let path = strip_lines(first);
            Some(WalkAnswerer {
                at_text: at.trim().to_string(),
                lang: Lang::of_path(&path),
                path,
                disposition: e
                    .get("disposition")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                reads: e
                    .get("reads")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect();

    // First-seen symbol order, and naming languages per symbol for the
    // same-language drop.
    let mut symbols: Vec<String> = Vec::new();
    let mut symbol_langs: BTreeMap<String, Vec<Lang>> = BTreeMap::new();
    // A changed code-file answerer that yields no symbol warns (five
    // questions, Q3: it never passes silently); a docs answerer stays quiet.
    let mut no_symbol: Vec<&WalkAnswerer> = Vec::new();
    for a in &walked {
        if !CHANGED.contains(&a.disposition.as_str()) {
            continue;
        }
        let Some(lang) = a.lang else { continue };
        let mut syms = at_symbol(&a.at_text).into_iter().collect::<Vec<_>>();
        syms.extend(reads_calls(&a.reads));
        syms.sort();
        syms.dedup();
        if syms.is_empty() {
            no_symbol.push(a);
            continue;
        }
        for sym in syms {
            if !symbols.contains(&sym) {
                symbols.push(sym.clone());
            }
            let langs = symbol_langs.entry(sym).or_default();
            if !langs.contains(&lang) {
                langs.push(lang);
            }
        }
    }

    // Coverage: every answerer's full path, any disposition.
    let covered: BTreeSet<String> = walked.iter().map(|a| a.path.clone()).collect();

    // Repo + base resolution. The walk reads the merge-base tree, so a plan's
    // own new files (which are not answerers that existed before it) never
    // look like unlisted readers of themselves.
    let repo_dir = match repo {
        Some(r) => {
            let dir = PathBuf::from(r);
            let top = git_output_in(&dir, &["rev-parse", "--show-toplevel"]);
            match top {
                Some(s) if !s.is_empty() => PathBuf::from(s),
                _ => {
                    warn_no_symbol(out, &no_symbol);
                    out.push_str("W\tcross-language walk NOT RUN (no git repo) - not a pass\n");
                    return;
                }
            }
        }
        None => match git_output(&["rev-parse", "--show-toplevel"], None) {
            Some(s) if !s.is_empty() => PathBuf::from(s),
            _ => {
                warn_no_symbol(out, &no_symbol);
                out.push_str("W\tcross-language walk NOT RUN (no git repo) - not a pass\n");
                return;
            }
        },
    };
    let resolved_base = match base {
        Some(b) => match git_output_in(&repo_dir, &["rev-parse", "--verify", "--quiet", b]) {
            Some(s) if !s.is_empty() => s,
            _ => {
                warn_no_symbol(out, &no_symbol);
                out.push_str(&format!(
                    "W\tcross-language walk NOT RUN (base {b} does not resolve) - not a pass\n"
                ));
                return;
            }
        },
        None => git_output_in(&repo_dir, &["merge-base", "HEAD", "origin/main"])
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "HEAD".to_string()),
    };

    if symbols.is_empty() {
        // Nothing to grep; the receipt still records that the trees were read.
        warn_no_symbol(out, &no_symbol);
        emit_receipt(out, &resolved_base, &symbols, &BTreeMap::new(), 0);
        return;
    }

    // One grep over every language tree at the base.
    let mut cmd_args: Vec<String> = vec!["grep".into(), "-o".into(), "-w".into(), "-F".into()];
    for sym in &symbols {
        cmd_args.push("-e".into());
        cmd_args.push(sym.clone());
    }
    cmd_args.push(resolved_base.clone());
    cmd_args.push("--".into());
    for g in [
        "*.py", "*.rs", "*.sh", "*.bash", "*.ts", "*.tsx", "*.js", "*.mjs", "*.go",
    ] {
        cmd_args.push(g.to_string());
    }
    let mut cmd = Command::new(git_bin_for_walk());
    cmd.current_dir(&repo_dir).args(&cmd_args);
    let grep = match cmd.output() {
        Ok(g) => g,
        Err(_) => {
            warn_no_symbol(out, &no_symbol);
            out.push_str(
                "W\tcross-language walk NOT RUN (git grep failed to start) - not a pass\n",
            );
            return;
        }
    };
    if grep.status.code().map(|c| c >= 2).unwrap_or(true) {
        warn_no_symbol(out, &no_symbol);
        out.push_str(&format!(
            "W\tcross-language walk NOT RUN (git grep exited {}) - not a pass\n",
            grep.status.code().unwrap_or(-1)
        ));
        return;
    }

    // Parse hits: `<sha>:<path>:<line>:<match>`. The rev prefix is the sha we
    // passed; rsplitn from the end leaves the path intact when it holds colons.
    let prefix = format!("{}:", resolved_base);
    let mut readers_by_tree: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut symbol_files: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if grep.status.success() {
        for line in String::from_utf8_lossy(&grep.stdout).lines() {
            let Some(rest) = line.strip_prefix(&prefix) else {
                continue;
            };
            // With `-o` there is no line number: `<sha>:<path>:<match>`, and
            // a fixed-string symbol never holds a colon, so the last field
            // splits the match from the path cleanly.
            let mut parts = rest.rsplitn(2, ':');
            let matched = parts.next().unwrap_or_default();
            let Some(path) = parts.next() else {
                continue;
            };
            if is_test_path(path) {
                continue;
            }
            let Some(lang) = Lang::of_path(path) else {
                continue;
            };
            readers_by_tree
                .entry(lang.name().to_string())
                .or_default()
                .insert(path.to_string());
            // A hit in a tree that names the symbol is the answerer's own tree;
            // a hit in a file some answerer names is already listed.
            if symbol_langs
                .get(matched)
                .map(|langs| langs.contains(&lang))
                .unwrap_or(false)
            {
                continue;
            }
            if covered.contains(path) {
                continue;
            }
            symbol_files
                .entry(matched.to_string())
                .or_default()
                .insert(path.to_string());
        }
    }

    // X/W lines per symbol, first-seen order, language groups sorted.
    let mut unlisted_total = 0usize;
    for sym in &symbols {
        let Some(files) = symbol_files.get(sym) else {
            continue;
        };
        unlisted_total += files.len();
        let mut by_lang: BTreeMap<&str, Vec<&String>> = BTreeMap::new();
        for f in files {
            if let Some(lang) = Lang::of_path(f) {
                by_lang.entry(lang.name()).or_default().push(f);
            }
        }
        // The cap is per symbol (the replay sized it against a symbol's total
        // unlisted files), not per language group; the lines still name each
        // tree's own count.
        let wide = files.len() > WIDE_SYMBOL_FILES;
        for (lang_name, group) in &by_lang {
            if wide {
                out.push_str(&format!(
                    "W\t`{sym}` is read in {lang_name} at {n} file(s) no answerer names. A \
                     symbol that wide is vocabulary, not one question's answerers: quote the \
                     call that answers the question in reads: instead.\n",
                    n = group.len()
                ));
            } else {
                out.push_str(&format!(
                    "X\t`{sym}` is read in {lang_name} at {n} file(s) no answerer names: \
                     {files}. Sweep that tree and list each reader with a disposition \
                     (out-of-scope with a reason when it does not answer this question).\n",
                    n = group.len(),
                    files = group
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }

    warn_no_symbol(out, &no_symbol);
    emit_receipt(
        out,
        &resolved_base,
        &symbols,
        &readers_by_tree,
        unlisted_total,
    );
}

// ── walk helpers ─────────────────────────────────────────────────────────────

/// The walk receipt: trees read, symbol count, unlisted-file total.
fn emit_receipt(
    out: &mut String,
    base: &str,
    symbols: &[String],
    readers: &BTreeMap<String, BTreeSet<String>>,
    unlisted: usize,
) {
    let trees: Vec<String> = readers
        .iter()
        .map(|(lang, files)| format!("{lang} {}", files.len()))
        .collect();
    let joined = if trees.is_empty() {
        String::new()
    } else {
        format!(" {}", trees.join(", "))
    };
    out.push_str(&format!(
        "O\tcross-language walk at {short}: {k} symbol(s), readers by tree{joined}; \
         {unlisted} unlisted\n",
        short = &base[..base.len().min(7)],
        k = symbols.len(),
    ));
}

/// One `W` line per changed code-file answerer that names no grep-able symbol
/// (five questions, Q3: it never passes silently). A docs answerer stays quiet.
fn warn_no_symbol(out: &mut String, answers: &[&WalkAnswerer]) {
    for a in answers {
        out.push_str(&format!(
            "W\t{} names no symbol for the cross-language walk - put the identifier after \
             the path in at: or quote a free name( call in reads:\n",
            a.at_text
        ));
    }
}

// ── symbol extraction ────────────────────────────────────────────────────────

/// The second `at:` token when it is a bare identifier:
/// `path[:lines] symbol (note)`.
fn ident_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap())
}

fn at_symbol(at_text: &str) -> Option<String> {
    let second = at_text.split_whitespace().nth(1)?;
    if !ident_re().is_match(second) {
        return None;
    }
    // The same keep-rule every symbol source obeys: a prose word from a
    // wordy `at:` note (`run and collect`) must not become a grep target.
    let trimmed = second.trim_matches('_');
    if trimmed.chars().count() >= 6 && trimmed.contains('_') {
        Some(second.to_string())
    } else {
        None
    }
}

/// An `at:` or `control:` value without its trailing ` (note)`.
fn at_head(at: &str) -> &str {
    at.split_once(" (").map_or(at, |(head, _)| head).trim()
}

/// Every free `name(` in `reads:`. There is no space before the parenthesis
/// (`provider_cap (file.py:1767)` is a line citation, not a call), a receiver
/// call (`path.is_file(`) is skipped, and a `::` path still counts. A name
/// survives only with >= 6 chars and an underscore once edge underscores are
/// trimmed.
fn call_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"([A-Za-z_][A-Za-z0-9_]*)\(").unwrap())
}

fn reads_calls(reads: &str) -> Vec<String> {
    let re = call_re();
    let bytes = reads.as_bytes();
    let mut out = Vec::new();
    for cap in re.captures_iter(reads) {
        let whole = cap.get(0).unwrap();
        let name = cap.get(1).unwrap().as_str();
        if whole.start() > 0 {
            let prev = bytes[whole.start() - 1];
            if prev == b'.' || prev == b'_' || prev.is_ascii_alphanumeric() {
                continue;
            }
        }
        let trimmed = name.trim_matches('_');
        if trimmed.chars().count() >= 6 && trimmed.contains('_') {
            out.push(name.to_string());
        }
    }
    out
}

// ── small helpers ────────────────────────────────────────────────────────────

/// The text between the first two lines that start with `---` - the same rule
/// every other check in the validator applies. `startswith`, so `---foo` opens
/// too, exactly like the Python.
fn frontmatter(content: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut opened: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("---") {
            match opened {
                None => opened = Some(i),
                Some(open) => {
                    return Some(lines[open + 1..i].join("\n"));
                }
            }
        }
    }
    None
}

fn nonempty_str(v: &Value) -> bool {
    v.as_str().map(|s| !s.trim().is_empty()).unwrap_or(false)
}

/// Python `isinstance(value, int) and not isinstance(value, bool)`.
fn is_int(v: &Value) -> bool {
    v.as_number()
        .map(|n| n.is_i64() || n.is_u64())
        .unwrap_or(false)
}

/// Render a value the way Python `%s` would: null as `None`, bools as
/// `True`/`False`, strings raw, containers as Python reprs.
fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else if let Some(f) = n.as_f64() {
                let s = format!("{f}");
                if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("NaN") {
                    s
                } else {
                    format!("{s}.0")
                }
            } else {
                n.to_string()
            }
        }
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(entries) => {
            let inner: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{}: {}", py_repr_str(k), py_repr(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

fn py_repr(v: &Value) -> String {
    match v {
        Value::String(s) => py_repr_str(s),
        other => py_str(other),
    }
}

/// Python repr of a string: single quotes unless the string holds one and no
/// double quote.
fn py_repr_str(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t");
    if escaped.contains('\'') && !escaped.contains('"') {
        format!("\"{escaped}\"")
    } else {
        format!("'{escaped}'")
    }
}

/// Strip a trailing `:<lines>` (`src/reader.py:10` -> `src/reader.py`).
fn strip_lines(token: &str) -> String {
    if let Some(pos) = token.rfind(':') {
        let suffix = &token[pos + 1..];
        if !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
            return token[..pos].to_string();
        }
    }
    token.to_string()
}

/// Drop test paths: anything under a test/fixtures dir, or named like a test
/// file, never answers a production question.
fn test_path_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(^|/)(tests?|fixtures|testdata)/").unwrap())
}

fn is_test_path(path: &str) -> bool {
    if test_path_re().is_match(path) {
        return true;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "tests.rs"
        || (name.starts_with("test_") && name.ends_with(".py"))
        || name.ends_with("_test.py")
        || name.ends_with("_test.go")
        || name.ends_with("_test.rs")
        || name.ends_with(".test.ts")
        || name.ends_with(".test.js")
}

// ── git plumbing ─────────────────────────────────────────────────────────────

fn git_output(args: &[&str], cwd: Option<&std::path::Path>) -> Option<String> {
    let mut cmd = Command::new(git_bin_for_walk());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.args(args);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_output_in(repo: &std::path::Path, args: &[&str]) -> Option<String> {
    git_output(args, Some(repo))
}

/// git by absolute path when resolvable, so a mid-run PATH flip cannot break
/// the walk (the same discipline git_test_helpers applies to tests).
fn git_bin_for_walk() -> PathBuf {
    std::env::split_paths(&std::env::var("PATH").unwrap_or_default())
        .map(|d| d.join("git"))
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("git"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(plan: &str, repo: Option<&str>, base: Option<&str>) -> String {
        let mut out = String::new();
        if shape_check(plan, &mut out) {
            cross_language_walk(plan, repo, base, &mut out);
        }
        out
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("surface-check-{}-{}", std::process::id(), tag));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    fn write_plan(dir: &std::path::Path, frontmatter: &str, name: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, format!("---\n{frontmatter}\n---\n\n# plan\n")).unwrap();
        path.to_string_lossy().to_string()
    }

    fn surface_block(question: &str, answerers: &str, count: usize, count_after: usize) -> String {
        format!(
            "status: ready\ncreated: 2026-09-20\nproject: fno\nsurface:\n  question: \
             \"{question}\"\n  sweep: \"rg -n 'sym' src/\"\n{answerers}  count: {count}\n  \
             count_after: {count_after}\n"
        )
    }

    const ONE_DUAL: &str = "  answerers:\n    - at: src/reader.py:1 row_ref_valid\n      \
        disposition: dual-logic\n      reads: \"row_ref_valid(row)\"\n      emits: \
        \"bool per row\"\n";

    // ── shape check parity ───────────────────────────────────────────────

    #[test]
    fn well_formed_block_prints_control_and_question_receipts() {
        let dir = tmp_dir("ok");
        let fm = surface_block(
            "Is this row reachable?",
            "  answerers:\n    - at: src/reader.py:10\n      disposition: dual-logic\n      \
             reads: \"if row.ref:\"\n      emits: \"12 rows\"\n    - at: src/writer.py:20\n      \
             disposition: out-of-scope\n      reason: \"writer emits, never reads\"\n  \
             control: src/reader.py:10\n",
            2,
            1,
        );
        let plan = write_plan(&dir, &fm, "ok.md");
        let out = run(&plan, None, None);
        assert!(
            out.contains("O\tcontrol `src/reader.py:10` returned by the sweep\n"),
            "{out}"
        );
        assert!(
            out.contains(
                "O\tquestion `Is this row reachable?`, 2 answerer(s), count 2, count_after 1\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn noun_phrase_question_fails_without_stopping_the_block() {
        let dir = tmp_dir("noun");
        let fm = surface_block("row reachability", ONE_DUAL, 1, 1);
        let plan = write_plan(&dir, &fm, "noun.md");
        let out = run(&plan, None, None);
        assert!(out.contains("does not end in a question mark"), "{out}");
        assert!(out.contains("O\tquestion `row reachability`"), "{out}");
    }

    #[test]
    fn empty_question_stops_before_the_sweep_tooth() {
        let dir = tmp_dir("emptyq");
        let fm = surface_block("   ", ONE_DUAL, 1, 1);
        let plan = write_plan(&dir, &fm, "emptyq.md");
        let out = run(&plan, None, None);
        assert_eq!(
            out,
            "E\tsurface.question is empty - phrase the question in one line\n"
        );
    }

    #[test]
    fn missing_surface_key_is_not_none() {
        let dir = tmp_dir("nosurf");
        let plan = write_plan(&dir, "status: ready\ncreated: 2026-09-20\n", "nosurf.md");
        let out = run(&plan, None, None);
        assert!(
            out.contains(
                "E\tsurface: must be a block of keys (question, sweep, answerers, count, \
                 count_after), not `None`\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn surface_list_renders_as_python_repr() {
        let dir = tmp_dir("list");
        let plan = write_plan(&dir, "status: ready\nsurface: [a, b]\n", "list.md");
        let out = run(&plan, None, None);
        assert!(out.contains("not `['a', 'b']`\n"), "{out}");
    }

    #[test]
    fn no_frontmatter_block_is_a_u_line() {
        let dir = tmp_dir("nofm");
        let plan = dir.join("nofm.md");
        std::fs::write(&plan, "# no frontmatter\n").unwrap();
        let out = run(plan.to_str().unwrap(), None, None);
        assert_eq!(out, "U\tno closed --- frontmatter block\n");
    }

    #[test]
    fn broken_yaml_is_a_clipped_u_line() {
        let dir = tmp_dir("badyaml");
        let plan = dir.join("badyaml.md");
        std::fs::write(&plan, "---\nsurface: [unclosed\n---\n").unwrap();
        let out = run(plan.to_str().unwrap(), None, None);
        assert!(out.starts_with("U\t"), "{out}");
        assert!(out.trim_end().chars().count() <= 161, "{out}");
    }

    #[test]
    fn count_teeth_render_python_none_and_stop() {
        let dir = tmp_dir("countnone");
        let fm = format!(
            "status: ready\ncreated: 2026-09-20\nsurface:\n  question: \"Is this \
             reachable?\"\n  sweep: \"rg\"\n  answerers:\n    - at: src/reader.py:1\n      \
             disposition: dual-logic\n      reads: \"x\"\n      emits: \"y\"\n  count: \
             null\n  count_after: 1\n"
        );
        let plan = write_plan(&dir, &fm, "countnone.md");
        let out = run(&plan, None, None);
        assert!(
            out.contains(
                "E\tsurface.count is `None` - an integer stating the PR estimate is the \
                 whole point of the block\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn count_mismatch_names_both_sides() {
        let dir = tmp_dir("countmm");
        let fm = surface_block(
            "Is this reachable?",
            "  answerers:\n    - at: src/reader.py:10\n      disposition: dual-logic\n      \
             reads: \"x\"\n      emits: \"y\"\n    - at: src/writer.py:20\n      \
             disposition: out-of-scope\n      reason: \"emits only\"\n",
            1,
            1,
        );
        let plan = write_plan(&dir, &fm, "countmm.md");
        let out = run(&plan, None, None);
        assert!(
            out.contains(
                "E\tsurface.count is 1 but the block lists 2 answerer(s) - the count is the \
                 estimate and must match what was found\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn count_after_missing_renders_none() {
        let dir = tmp_dir("cafter");
        let fm =
            surface_block("Is this reachable?", ONE_DUAL, 1, 1).replace("  count_after: 1\n", "");
        let plan = write_plan(&dir, &fm, "cafter.md");
        let out = run(&plan, None, None);
        assert!(
            out.contains(
                "E\tsurface.count_after is `None` - state in a number how many answerers \
                 survive this plan"
            ),
            "{out}"
        );
    }

    #[test]
    fn count_after_above_count_fails() {
        let dir = tmp_dir("caexc");
        let fm = surface_block("Is this reachable?", ONE_DUAL, 1, 3);
        let plan = write_plan(&dir, &fm, "caexc.md");
        let out = run(&plan, None, None);
        assert!(
            out.contains(
                "E\tsurface.count_after (3) exceeds count (1) - a plan cannot leave more \
                 answerers than it found\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn undisposed_names_site_and_reason_teeth() {
        let dir = tmp_dir("undisp");
        let fm = surface_block(
            "Is this reachable?",
            "  answerers:\n    - at: src/reader.py:10\n      disposition: dual-logic\n      \
             reads: \"x\"\n      emits: \"y\"\n    - at: src/writer.py:20\n      \
             disposition: out-of-scope\n",
            2,
            1,
        );
        let plan = write_plan(&dir, &fm, "undisp.md");
        let out = run(&plan, None, None);
        assert!(
            out.contains("Undisposed: src/writer.py:20 (out-of-scope with no reason)."),
            "{out}"
        );
        assert!(
            out.contains("has 2 answerers; the plan disposes of 1."),
            "{out}"
        );
    }

    #[test]
    fn bool_disposition_renders_python_way() {
        let dir = tmp_dir("booldisp");
        let fm = surface_block(
            "Is this reachable?",
            "  answerers:\n    - at: src/reader.py:10\n      disposition: true\n      \
             reads: \"x\"\n      emits: \"y\"\n",
            1,
            1,
        );
        let plan = write_plan(&dir, &fm, "booldisp.md");
        let out = run(&plan, None, None);
        assert!(
            out.contains(
                "disposition `True` is not one of dual-logic | shared-vocabulary | \
                 generated-artifact | out-of-scope"
            ),
            "{out}"
        );
    }

    #[test]
    fn changed_answerer_without_feed_names_the_site() {
        let dir = tmp_dir("nofeed");
        let fm = surface_block(
            "Is this reachable?",
            "  answerers:\n    - at: src/reader.py:10\n      disposition: dual-logic\n      \
             emits: \"12 rows\"\n",
            1,
            1,
        );
        let plan = write_plan(&dir, &fm, "nofeed.md");
        let out = run(&plan, None, None);
        assert!(
            out.contains("E\tPlan changes src/reader.py:10 but names no feed for it."),
            "{out}"
        );
    }

    #[test]
    fn phantom_control_fails() {
        let dir = tmp_dir("ctrl");
        let fm = surface_block(
            "Is this reachable?",
            "  answerers:\n    - at: src/reader.py:10\n      disposition: dual-logic\n      \
             reads: \"x\"\n      emits: \"y\"\n  control: src/nowhere.py:1\n",
            1,
            1,
        );
        let plan = write_plan(&dir, &fm, "ctrl.md");
        let out = run(&plan, None, None);
        assert!(out.contains("names no listed answerer"), "{out}");
        assert!(out.contains("`src/reader.py:10`"), "{out}");
    }

    #[test]
    fn control_matches_answerer_ignoring_at_note() {
        let dir = tmp_dir("ctrl-head");
        let fm = surface_block(
            "Is this reachable?",
            "  answerers:\n    - at: src/reader.py:10 row_ref_valid (the filter)\n      \
             disposition: dual-logic\n      reads: \"x\"\n      emits: \"y\"\n  \
             control: src/reader.py:10 row_ref_valid\n",
            1,
            1,
        );
        let plan = write_plan(&dir, &fm, "ctrl-head.md");
        let out = run(&plan, None, None);
        assert!(
            out.contains("O\tcontrol `src/reader.py:10 row_ref_valid` returned by the sweep"),
            "{out}"
        );
        assert!(!out.contains("names no listed answerer"), "{out}");
    }

    #[test]
    fn quick_template_surface_example_passes() {
        let tpl = include_str!("../../../skills/blueprint/references/quick-template.md");
        let fm = tpl
            .split("```markdown\n---\n")
            .nth(1)
            .and_then(|s| s.split("\n---\n").next())
            .unwrap();
        let dir = tmp_dir("tpl");
        let plan = write_plan(&dir, fm, "tpl.md");
        let out = run(&plan, None, None);
        assert!(out.contains("O\tcontrol"), "{out}");
        assert!(!out.contains("names no listed answerer"), "{out}");
    }

    // ── cross-language walk ──────────────────────────────────────────────

    /// A temp git repo: `src/reader.py` defines `row_ref_valid`; `crates/x/`
    /// holds Rust readers calling it. Committed; the walk greps the tree.
    fn walk_repo(tag: &str, extras: &[(&str, &str)]) -> PathBuf {
        let dir = tmp_dir(tag);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("crates/x/src")).unwrap();
        std::fs::write(
            dir.join("src/reader.py"),
            "def row_ref_valid(row):\n    return bool(row)\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("crates/x/src/reader.rs"),
            "fn go(row: i32) -> bool {\n    row_ref_valid(row)\n}\n",
        )
        .unwrap();
        for (path, body) in extras {
            let full = dir.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }
        assert!(crate::git_test_helpers::git_init(&dir));
        let mut adds = vec!["src/reader.py", "crates/x/src/reader.rs"];
        adds.extend(extras.iter().map(|(p, _)| p));
        for add in adds {
            assert!(crate::git_test_helpers::git_run(&["add", add], &dir)
                .unwrap()
                .status
                .success());
        }
        assert!(crate::git_test_helpers::git_run(
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "init"
            ],
            &dir
        )
        .unwrap()
        .status
        .success());
        dir
    }

    fn run_walk(plan: &str, repo: &std::path::Path) -> String {
        run(plan, Some(repo.to_str().unwrap()), Some("HEAD"))
    }

    #[test]
    fn walk_refuses_rust_reader_a_python_answerer_does_not_name() {
        let repo = walk_repo("walkx", &[]);
        let plan = write_plan(
            &repo,
            &surface_block("Is this row reachable?", ONE_DUAL, 1, 1),
            "p.md",
        );
        let out = run_walk(&plan, &repo);
        assert!(
            out.contains(
                "X\t`row_ref_valid` is read in rust at 1 file(s) no answerer names: \
                 crates/x/src/reader.rs."
            ),
            "{out}"
        );
        assert!(out.contains("cross-language walk at "), "{out}");
    }

    #[test]
    fn out_of_scope_answerer_covers_the_file_and_receipt_names_both_trees() {
        let repo = walk_repo("walkc", &[]);
        let fm = surface_block(
            "Is this row reachable?",
            "  answerers:\n    - at: src/reader.py:1 row_ref_valid\n      disposition: \
             dual-logic\n      reads: \"row_ref_valid(row)\"\n      emits: \"bool per \
             row\"\n    - at: crates/x/src/reader.rs:2\n      disposition: out-of-scope\n      \
             reason: \"calls the python verdict, never reads the row\"\n",
            2,
            2,
        );
        let plan = write_plan(&repo, &fm, "p.md");
        let out = run_walk(&plan, &repo);
        assert!(!out.contains("X\t"), "{out}");
        assert!(
            out.contains("readers by tree python 1, rust 1; 0 unlisted"),
            "{out}"
        );
    }

    #[test]
    fn test_paths_and_same_language_hits_never_refuse() {
        let repo = walk_repo(
            "walkt",
            &[
                (
                    "tests/test_reader.py",
                    "def check():\n    row_ref_valid(1)\n",
                ),
                ("src/other.py", "def wrap():\n    return row_ref_valid(2)\n"),
            ],
        );
        let plan = write_plan(
            &repo,
            &surface_block("Is this row reachable?", ONE_DUAL, 1, 1),
            "p.md",
        );
        let out = run_walk(&plan, &repo);
        assert!(!out.contains("src/other.py"), "{out}");
        assert!(!out.contains("tests/test_reader.py"), "{out}");
        assert!(out.contains("crates/x/src/reader.rs"), "{out}");
        assert!(
            out.contains("readers by tree python 2, rust 1; 1 unlisted"),
            "{out}"
        );
    }

    #[test]
    fn receiver_calls_and_line_citations_yield_no_symbol() {
        let repo = walk_repo("walkr", &[]);
        let fm = surface_block(
            "Is this row reachable?",
            "  answerers:\n    - at: src/reader.py:1 row_ref_valid\n      disposition: \
             dual-logic\n      reads: \"path.is_file(provider_cap (src/reader.py:1))\"\n      \
             emits: \"bool per row\"\n",
            1,
            1,
        );
        let plan = write_plan(&repo, &fm, "p.md");
        let out = run_walk(&plan, &repo);
        assert!(!out.contains("`is_file`"), "{out}");
        assert!(!out.contains("`provider_cap`"), "{out}");
        assert!(out.contains("`row_ref_valid`"), "{out}");
    }

    #[test]
    fn module_qualified_call_is_skipped() {
        let repo = walk_repo("walkm", &[]);
        let fm = surface_block(
            "Is this row reachable?",
            "  answerers:\n    - at: src/reader.py:1 row_ref_valid\n      disposition: \
             dual-logic\n      reads: \"spawn_gate.provider_live_count(row)\"\n      emits: \
             \"int\"\n",
            1,
            1,
        );
        let plan = write_plan(&repo, &fm, "p.md");
        let out = run_walk(&plan, &repo);
        assert!(!out.contains("`provider_live_count`"), "{out}");
    }

    #[test]
    fn wide_symbol_warns_as_vocabulary_never_refuses() {
        let mut extras = Vec::new();
        for i in 0..12 {
            extras.push((
                format!("crates/x/src/m{i}.rs"),
                format!("fn f{i}() {{ row_ref_valid({i}); }}\n"),
            ));
        }
        let refs: Vec<(&str, &str)> = extras
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let repo = walk_repo("walkw", &refs);
        let plan = write_plan(
            &repo,
            &surface_block("Is this row reachable?", ONE_DUAL, 1, 1),
            "p.md",
        );
        let out = run_walk(&plan, &repo);
        assert!(
            out.contains("W\t`row_ref_valid` is read in rust at 13 file(s)"),
            "{out}"
        );
        assert!(!out.contains("X\t"), "{out}");
    }

    #[test]
    fn wide_cap_triggers_on_the_symbol_total_across_trees() {
        let mut extras = Vec::new();
        for i in 0..6 {
            extras.push((
                format!("crates/x/src/r{i}.rs"),
                format!("fn r{i}() {{ row_ref_valid({i}); }}\n"),
            ));
            extras.push((
                format!("scripts/g{i}.sh"),
                format!("# shell read\nrow_ref_valid {i}\n"),
            ));
        }
        let refs: Vec<(&str, &str)> = extras
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let repo = walk_repo("walkw2", &refs);
        let plan = write_plan(
            &repo,
            &surface_block("Is this row reachable?", ONE_DUAL, 1, 1),
            "p.md",
        );
        let out = run_walk(&plan, &repo);
        // 12 unlisted files total: over the cap per symbol, so W lines for
        // both trees and no X line anywhere.
        assert!(
            out.contains("W\t`row_ref_valid` is read in rust at 7 file(s)"),
            "{out}"
        );
        assert!(
            out.contains("W\t`row_ref_valid` is read in shell at 6 file(s)"),
            "{out}"
        );
        assert!(!out.contains("X\t"), "{out}");
    }

    #[test]
    fn walk_outside_any_repo_warns_not_run() {
        let dir = tmp_dir("norepo");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let plan = write_plan(
            &dir,
            &surface_block("Is this row reachable?", ONE_DUAL, 1, 1),
            "p.md",
        );
        let out = run(&plan, Some(dir.to_str().unwrap()), Some("HEAD"));
        assert!(
            out.contains("W\tcross-language walk NOT RUN (no git repo) - not a pass"),
            "{out}"
        );
        assert!(!out.contains("cross-language walk at "), "{out}");
    }

    #[test]
    fn unresolvable_base_warns_not_run() {
        let repo = walk_repo("walkb", &[]);
        let plan = write_plan(
            &repo,
            &surface_block("Is this row reachable?", ONE_DUAL, 1, 1),
            "p.md",
        );
        let out = run(&plan, Some(repo.to_str().unwrap()), Some("no-such-ref"));
        assert!(
            out.contains(
                "W\tcross-language walk NOT RUN (base no-such-ref does not resolve) - not \
                 a pass"
            ),
            "{out}"
        );
    }

    #[test]
    fn code_answerer_without_any_symbol_warns() {
        let repo = walk_repo("walkn", &[]);
        let fm = surface_block(
            "Is this row reachable?",
            "  answerers:\n    - at: src/reader.py:1\n      disposition: dual-logic\n      \
             reads: \"the if on line 1\"\n      emits: \"bool per row\"\n",
            1,
            1,
        );
        let plan = write_plan(&repo, &fm, "p.md");
        let out = run_walk(&plan, &repo);
        assert!(
            out.contains("W\tsrc/reader.py:1 names no symbol for the cross-language walk"),
            "{out}"
        );
    }

    #[test]
    fn full_path_coverage_not_basename() {
        let repo = walk_repo(
            "walkp",
            &[
                ("crates/y/src/lib.rs", "fn a() { row_ref_valid(3); }\n"),
                ("crates/z/src/lib.rs", "fn b() { row_ref_valid(4); }\n"),
            ],
        );
        let fm = surface_block(
            "Is this row reachable?",
            "  answerers:\n    - at: src/reader.py:1 row_ref_valid\n      disposition: \
             dual-logic\n      reads: \"row_ref_valid(row)\"\n      emits: \"bool per \
             row\"\n    - at: crates/y/src/lib.rs:1\n      disposition: out-of-scope\n      \
             reason: \"wrapper\"\n",
            2,
            2,
        );
        let plan = write_plan(&repo, &fm, "p.md");
        let out = run_walk(&plan, &repo);
        // crates/y/src/lib.rs is covered by its full path; crates/z/src/lib.rs
        // shares only the basename, so it must still be named.
        assert!(out.contains("crates/z/src/lib.rs"), "{out}");
        assert!(
            !out.contains("no answerer names: crates/y/src/lib.rs"),
            "{out}"
        );
        assert!(
            out.contains("readers by tree python 1, rust 3; 2 unlisted"),
            "{out}"
        );
    }

    #[test]
    fn short_or_underscoreless_names_are_dropped() {
        let calls = reads_calls("use count(1); then row_ref_valid(2)");
        assert_eq!(calls, vec!["row_ref_valid".to_string()]);
        assert!(reads_calls("path.is_file(").is_empty());
        assert!(reads_calls("_private(").is_empty());
        assert!(reads_calls("getCount(").is_empty());
    }

    #[test]
    fn at_symbol_takes_only_bare_identifier() {
        assert_eq!(
            at_symbol("src/reader.py:1 row_ref_valid"),
            Some("row_ref_valid".into())
        );
        assert_eq!(at_symbol("src/reader.py"), None);
        assert_eq!(at_symbol("src/reader.py:10"), None);
        // Prose words from a wordy at: note never become grep targets.
        assert_eq!(at_symbol("src/reader.py:1 run and collect"), None);
        assert_eq!(at_symbol("src/reader.py:1 and"), None);
    }

    #[test]
    fn strip_lines_takes_lines_suffix_off() {
        assert_eq!(strip_lines("src/reader.py:10"), "src/reader.py");
        assert_eq!(strip_lines("src/reader.py:10-20"), "src/reader.py");
        assert_eq!(strip_lines("src/reader.py"), "src/reader.py");
    }
}
