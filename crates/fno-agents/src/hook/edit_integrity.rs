//! PostToolUse edit-integrity checks over the paths one Edit, Write or
//! apply_patch call wrote: parse, last line, test count, stale references.
//!
//! Every check compares against a baseline: the `--before` text when the
//! shim hands one (claude's `tool_response.originalFile`), else
//! `git show <base>:<path>`. A failed read or subprocess failure skips
//! that one check; the entry never panics and has no refusal in it - the
//! edit already happened, so every finding is advice the model reads as
//! additionalContext.
//! Shell shim: `hooks/edit-integrity.sh`.

use regex::Regex;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One line per finding on stdout, `edit-integrity: <rel>: <message>`.
/// Exit 0 with no blocking finding, 1 with one, 2 on usage.
pub fn run(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(()) => {
            eprintln!(
                "usage: fno-agents hook edit-integrity [--base <base>] [--before <file>] [--] <path>..."
            );
            return 2;
        }
    };
    let findings = check_paths(&parsed.paths, &parsed.base, parsed.before.as_deref());
    let blocking = findings.iter().any(|f| is_blocking(f));
    print_findings(&findings);
    if blocking {
        1
    } else {
        0
    }
}

fn print_findings(findings: &[String]) {
    let mut stdout = std::io::stdout().lock();
    for line in findings {
        let _ = writeln!(stdout, "{line}");
    }
}

/// Parse/stale findings block (exit 1); advice (last line, test count)
/// does not. Read off the emitted line so the classification lives next to
/// the message text.
fn is_blocking(line: &str) -> bool {
    line.contains(": does not parse: ") || line.contains(": removed top-level ")
}

struct Parsed {
    base: String,
    before: Option<PathBuf>,
    paths: Vec<PathBuf>,
}

/// The one legal `Err(())`: usage failure - no path, a dangling flag
/// value, `--before` with several paths, or an unknown flag.
fn parse_args(args: &[String]) -> Result<Parsed, ()> {
    let mut base = "HEAD".to_string();
    let mut before: Option<PathBuf> = None;
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut only_paths = false;
    let mut i = 0;
    while i < args.len() {
        let tok = &args[i];
        if only_paths {
            paths.push(PathBuf::from(tok));
        } else if tok == "--" {
            only_paths = true;
        } else if tok == "--base" {
            i += 1;
            base = args.get(i).ok_or(())?.clone();
        } else if tok == "--before" {
            i += 1;
            before = Some(PathBuf::from(args.get(i).ok_or(())?));
        } else if tok.starts_with('-') && tok.len() > 1 {
            return Err(());
        } else {
            paths.push(PathBuf::from(tok));
        }
        i += 1;
    }
    if paths.is_empty() || (before.is_some() && paths.len() > 1) {
        return Err(());
    }
    Ok(Parsed {
        base,
        before,
        paths,
    })
}

fn check_paths(paths: &[PathBuf], base: &str, before: Option<&Path>) -> Vec<String> {
    let mut out = Vec::new();
    for path in paths {
        let baseline = baseline_for(path, base, before);
        out.extend(check_one(path, baseline));
    }
    out
}

struct Baseline {
    text: String,
    /// What messages name as the comparison point: "HEAD", another rev, or
    /// "before this edit".
    label: String,
}

/// The pre-edit text of one path: the `--before` file when given, else the
/// base revision's copy. None when the file is new, untracked, outside a
/// repo, or a read fails - the checks then treat the file as new.
fn baseline_for(path: &Path, base: &str, before: Option<&Path>) -> Option<Baseline> {
    if let Some(file) = before {
        return std::fs::read_to_string(file).ok().map(|text| Baseline {
            text,
            label: "before this edit".to_string(),
        });
    }
    let abs = absolutize(path);
    let dir = abs.parent().unwrap_or(Path::new("."));
    let root = repo_root(dir)?;
    // git prints the canonical root (/private/var on macOS) while the path
    // may arrive through a symlink (/var), so compare canonical to canonical.
    let canonical = std::fs::canonicalize(&abs).unwrap_or_else(|_| abs.clone());
    let rel = canonical
        .strip_prefix(&root)
        .ok()?
        .to_string_lossy()
        .replace('\\', "/");
    let out = Command::new("git")
        .arg("-C")
        .arg(&root)
        .arg("show")
        .arg(format!("{base}:{rel}"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(Baseline {
        text: String::from_utf8_lossy(&out.stdout).into_owned(),
        label: base.to_string(),
    })
}

fn check_one(path: &Path, baseline: Option<Baseline>) -> Vec<String> {
    let mut out = Vec::new();
    let abs = absolutize(path);
    if !abs.is_file() {
        return out;
    }
    let Some((text, rel)) = read_text(&abs) else {
        return out;
    };
    let prefix = format!("edit-integrity: {rel}: ");
    let ext = abs
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();

    if let Some(message) = parse_finding(&text, &ext, &abs, baseline.as_ref()) {
        out.push(format!("{prefix}does not parse: {message}"));
    }
    if let Some(message) = last_line_finding(baseline.as_ref(), &text) {
        out.push(format!("{prefix}{message}"));
    }
    if let Some(message) = test_count_finding(&ext, baseline.as_ref(), &text) {
        out.push(format!("{prefix}{message}"));
    }
    if ext == "py" {
        if let Some(baseline) = baseline.as_ref() {
            if let Some(root) = repo_root(abs.parent().unwrap_or(Path::new("."))) {
                for message in stale_reference_findings(&root, &rel, baseline, &text) {
                    out.push(format!("{prefix}{message}"));
                }
            }
        }
    }
    out
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

/// The file's text when it is a readable text file: a NUL byte in the
/// first 8 KiB (a binary) skips every check. A first read that is empty or
/// unterminated is read once more 50 ms later - format-on-edit may still
/// be rewriting the same file in parallel.
fn read_text(abs: &Path) -> Option<(String, String)> {
    let bytes = std::fs::read(abs).ok()?;
    let binary = bytes
        .chunks(8192)
        .next()
        .map(|head| head.contains(&0))
        .unwrap_or(false);
    if binary {
        return None;
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if text.is_empty() || !text.ends_with('\n') {
        std::thread::sleep(std::time::Duration::from_millis(50));
        text = String::from_utf8_lossy(&std::fs::read(abs).ok()?).into_owned();
    }
    Some((text, repo_relative(abs)))
}

/// The path as the message shows it: repo-relative when the file sits in a
/// repo, the absolute path otherwise. Canonicalized the same way
/// `baseline_for` is, so the /var to /private/var symlink never splits the
/// two reads.
fn repo_relative(abs: &Path) -> String {
    if let Some(dir) = abs.parent() {
        if let Some(root) = repo_root(dir) {
            let canonical = std::fs::canonicalize(abs).unwrap_or_else(|_| abs.to_path_buf());
            if let Ok(rel) = canonical.strip_prefix(&root) {
                return rel.to_string_lossy().replace('\\', "/");
            }
        }
    }
    abs.to_string_lossy().into_owned()
}

fn repo_root(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

// --- check 1: parse (blocking) ---

/// The parser's first error line, or None when the text parses, the
/// extension has no parser here, the tool is missing, or the gate holds
/// the finding back. The gate: report only when the baseline exists and
/// parsed (an edit broke a file that was whole), or the file is new
/// outside `fixtures/` and `testdata/` (deliberately invalid fixtures
/// never fire).
fn parse_finding(text: &str, ext: &str, abs: &Path, baseline: Option<&Baseline>) -> Option<String> {
    let failure = first_parse_failure(text, ext, Some(abs))?;
    let allowed = match baseline {
        Some(b) => first_parse_failure(&b.text, ext, None).is_none(),
        None => !under_fixture_dir(abs),
    };
    allowed.then_some(failure)
}

/// Run the one parser for `ext` over `text` and answer the first error
/// line. `path` is handed to the subprocess parsers that read a file; the
/// baseline parse reuses the stdin forms, so None is legal there.
fn first_parse_failure(text: &str, ext: &str, path: Option<&Path>) -> Option<String> {
    match ext {
        "py" => {
            let source = if path.is_some() {
                "import ast, sys
try:
    ast.parse(open(sys.argv[1], 'rb').read(), sys.argv[1])
except Exception as e:
    sys.stderr.write(f'{type(e).__name__}: {e}')
    sys.exit(1)"
            } else {
                "import ast, sys
try:
    ast.parse(sys.stdin.buffer.read(), '<baseline>')
except Exception as e:
    sys.stderr.write(f'{type(e).__name__}: {e}')
    sys.exit(1)"
            };
            let mut cmd = Command::new("python3");
            cmd.arg("-c").arg(source);
            if let Some(path) = path {
                cmd.arg(path);
            } else {
                cmd.stdin(std::process::Stdio::piped());
            }
            let mut child = cmd.stderr(std::process::Stdio::piped()).spawn().ok()?;
            if path.is_none() {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(text.as_bytes());
                }
            }
            let out = child.wait_with_output().ok()?;
            (!out.status.success()).then(|| first_line(&String::from_utf8_lossy(&out.stderr)))
        }
        "json" => serde_json::from_str::<serde_json::Value>(text)
            .err()
            .map(|e| first_line(&e.to_string())),
        "toml" => toml::from_str::<toml::Value>(text)
            .err()
            .map(|e| first_line(&e.to_string())),
        "yaml" | "yml" => serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text)
            .err()
            .map(|e| first_line(&e.to_string())),
        "sh" | "bash" => {
            let mut cmd = Command::new("bash");
            cmd.arg("-n");
            if let Some(path) = path {
                cmd.arg(path);
            } else {
                cmd.stdin(std::process::Stdio::piped());
            }
            let mut child = cmd.stderr(std::process::Stdio::piped()).spawn().ok()?;
            if path.is_none() {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(text.as_bytes());
                }
            }
            let out = child.wait_with_output().ok()?;
            (!out.status.success()).then(|| first_line(&String::from_utf8_lossy(&out.stderr)))
        }
        _ => None,
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("parse failed")
        .to_string()
}

/// True when any path segment is `fixtures` or `testdata`: a new invalid
/// file there is a deliberate fixture, never a finding.
fn under_fixture_dir(abs: &Path) -> bool {
    abs.iter().any(|seg| {
        let seg = seg.to_string_lossy();
        seg == "fixtures" || seg == "testdata"
    })
}

// --- check 2: last line (advisory) ---

fn last_line_finding(baseline: Option<&Baseline>, text: &str) -> Option<String> {
    if text.is_empty() {
        return baseline.filter(|b| !b.text.is_empty()).map(|b| {
            format!(
                "file is now empty; {} had {} lines.",
                b.label,
                b.text.lines().count()
            )
        });
    }
    if text.ends_with('\n') {
        return None;
    }
    match baseline {
        Some(b) if !b.text.is_empty() && !b.text.ends_with('\n') => None,
        Some(b) => Some(format!(
            "last line has no newline and {} did; the write may have been cut short. Re-read the end of the file.",
            b.label
        )),
        None => Some(
            "last line has no newline; the write may have been cut short. Re-read the end of the file."
                .to_string(),
        ),
    }
}

// --- check 3: test count (advisory) ---

fn test_count_finding(ext: &str, baseline: Option<&Baseline>, text: &str) -> Option<String> {
    let baseline = baseline?;
    let (count, baseline_count) = match ext {
        "py" => (count_py_tests(text), count_py_tests(&baseline.text)),
        "rs" => (count_rs_tests(text), count_rs_tests(&baseline.text)),
        _ => return None,
    };
    if baseline_count > count {
        Some(format!(
            "test count fell {baseline_count} -> {count} against {}. AGENTS.md: read the exact range, edit, re-read, and prove the test count before and after. If the drop is intended, ignore this.",
            baseline.label
        ))
    } else {
        None
    }
}

fn count_py_tests(text: &str) -> usize {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?m)^\s*(?:async\s+)?def\s+test_").expect("regex"));
    re.find_iter(text).count()
}

fn count_rs_tests(text: &str) -> usize {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"#\[(?:\w+::)*test\]").expect("regex"));
    re.find_iter(text).count()
}

// --- check 4: stale references (blocking, .py with a baseline) ---

/// Findings for baseline top-level defs and classes the new text no longer
/// names as a whole word, where the repo still names them under the
/// module's dotted path.
fn stale_reference_findings(
    root: &Path,
    rel: &str,
    baseline: &Baseline,
    text: &str,
) -> Vec<String> {
    if !rel.ends_with(".py") {
        return Vec::new();
    }
    let Some(dotted) = dotted_module(rel) else {
        return Vec::new();
    };
    let removed = removed_top_level_names(&baseline.text, text);
    if removed.is_empty() {
        return Vec::new();
    }
    let hits = grep_dotted(root, &dotted);
    let mut out = Vec::new();
    for name in removed {
        let mut shown = 0usize;
        let mut total = 0usize;
        for hit in &hits {
            let Some((file, lineno, line)) = split_grep_hit(hit) else {
                continue;
            };
            if file == rel {
                continue;
            }
            let Some((hit_file, hit_lineno, hit_line)) =
                stale_hit_at(file, lineno, line, &dotted, &name, root)
            else {
                continue;
            };
            total += 1;
            if shown < 10 {
                out.push(format!(
                    "removed top-level {name}; still named at {hit_file}:{hit_lineno}: {hit_line}"
                ));
                shown += 1;
            }
        }
        if total > shown {
            out.push(format!(
                "removed top-level {name}; ... and {} more",
                total - shown
            ));
        }
    }
    out
}

/// The `git grep -n -F` output lines for the dotted module, one call for
/// every removed name; a grep that fails (no matches) answers empty.
fn grep_dotted(root: &Path, dotted: &str) -> Vec<String> {
    match Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("grep")
        .arg("-n")
        .arg("-F")
        .arg("-e")
        .arg(dotted)
        .arg("--")
        .arg("*.py")
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// `file:lineno:line` for one `git grep -n` hit; the content may hold
/// colons.
fn split_grep_hit(hit: &str) -> Option<(&str, usize, &str)> {
    let (file, rest) = hit.split_once(':')?;
    let (lineno, line) = rest.split_once(':')?;
    Some((file, lineno.parse().ok()?, line))
}

/// The finding triple for this grep hit when it still names `name`: a
/// `<dotted>.<name>` whole word on the line, a one-line
/// `from <dotted> import` naming it, or a `from <dotted> import (` block
/// whose body names it (read forward to the closing paren).
fn stale_hit_at(
    file: &str,
    lineno: usize,
    line: &str,
    dotted: &str,
    name: &str,
    root: &Path,
) -> Option<(String, usize, String)> {
    let escaped = regex::escape(&format!("{dotted}.{name}"));
    let re = Regex::new(&format!(r"\b{escaped}\b")).ok()?;
    if re.is_match(line) {
        return Some((file.to_string(), lineno, line.trim().to_string()));
    }
    if one_line_import_hits(line, dotted, name) {
        return Some((file.to_string(), lineno, line.trim().to_string()));
    }
    if let Some((body_lineno, body_line)) =
        paren_import_body_hits(file, lineno, line, dotted, name, root)
    {
        return Some((file.to_string(), body_lineno, body_line));
    }
    None
}

/// True when the trimmed hit line opens a `from <dotted> import` and
/// already names `name` on the same line.
fn one_line_import_hits(line: &str, dotted: &str, name: &str) -> bool {
    let prefix = format!("from {dotted} import");
    let trimmed = line.trim();
    if !trimmed.starts_with(&prefix) {
        return false;
    }
    let rest = trimmed[prefix.len()..].trim_start();
    if rest.starts_with('(') {
        return false;
    }
    let Ok(name_re) = Regex::new(&format!(r"\b{}\b", regex::escape(name))) else {
        return false;
    };
    name_re.is_match(rest)
}

/// The body line that names `name`, when the trimmed hit line opens a
/// `from <dotted> import (` block: read the file forward from the opening
/// line to the closing paren and answer the first line that names it.
fn paren_import_body_hits(
    file: &str,
    lineno: usize,
    line: &str,
    dotted: &str,
    name: &str,
    root: &Path,
) -> Option<(usize, String)> {
    let prefix = format!("from {dotted} import");
    let trimmed = line.trim();
    if !trimmed.starts_with(&prefix) {
        return None;
    }
    if !trimmed[prefix.len()..].trim_start().starts_with('(') {
        return None;
    }
    let Ok(name_re) = Regex::new(&format!(r"\b{}\b", regex::escape(name))) else {
        return None;
    };
    let body = read_file_lines(root, file)?;
    for (i, body_line) in body.iter().enumerate().skip(lineno.saturating_sub(1)) {
        if i > lineno.saturating_sub(1) && body_line.trim().starts_with(')') {
            break;
        }
        if name_re.is_match(body_line) {
            return Some((i + 1, body_line.trim().to_string()));
        }
    }
    None
}

fn read_file_lines(root: &Path, rel: &str) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(root.join(rel)).ok()?;
    Some(text.lines().map(str::to_string).collect())
}

/// The module's dotted path from its repo-relative path: the part after
/// the last `src/` segment, `.py` dropped, a trailing `/__init__` dropped,
/// `/` replaced with `.`. None when a segment cannot be a Python
/// identifier (the mapping would guess a module that does not exist).
fn dotted_module(rel: &str) -> Option<String> {
    let after_src = match rel.rfind("src/") {
        Some(i) => &rel[i + 4..],
        None => rel,
    };
    let stem = after_src.strip_suffix(".py")?;
    let stem = stem.strip_suffix("/__init__").unwrap_or(stem);
    let dotted = stem.replace('/', ".");
    let identifier = |seg: &str| {
        let mut chars = seg.chars();
        matches!(
            chars.next(),
            Some(c) if c.is_ascii_alphabetic() || c == '_'
        ) && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    dotted.split('.').all(identifier).then_some(dotted)
}

/// Baseline top-level defs and classes absent as a whole word from the new
/// text. A name kept by a re-export import still appears, so it never
/// fires; a rename lands under the new name and the old one drops out.
fn removed_top_level_names(before: &str, after: &str) -> Vec<String> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?m)^(?:async\s+def|def|class)\s+([A-Za-z_]\w*)").expect("regex")
    });
    let mut removed = Vec::new();
    for cap in re.captures_iter(before) {
        let name = &cap[1];
        let Ok(word) = Regex::new(&format!(r"\b{}\b", regex::escape(name))) else {
            continue;
        };
        if !word.is_match(after) && !removed.iter().any(|n: &String| n == name) {
            removed.push(name.to_string());
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(n: usize) -> String {
        (0..n).map(|i| format!("line {i}\n")).collect()
    }

    fn baseline(text: &str, label: &str) -> Baseline {
        Baseline {
            text: text.to_string(),
            label: label.to_string(),
        }
    }

    #[test]
    fn dotted_module_maps_src_layout() {
        assert_eq!(
            dotted_module("cli/src/fno/demo/helpers.py"),
            Some("fno.demo.helpers".to_string())
        );
    }

    #[test]
    fn dotted_module_drops_init() {
        assert_eq!(
            dotted_module("cli/src/fno/__init__.py"),
            Some("fno".to_string())
        );
    }

    #[test]
    fn dotted_module_skips_non_identifier_segments() {
        assert_eq!(dotted_module("src/my-mod/helpers.py"), None);
        assert!(dotted_module("src/x/y.py").is_some());
    }

    #[test]
    fn dotted_module_without_src_uses_whole_path() {
        assert_eq!(
            dotted_module("demo/helpers.py"),
            Some("demo.helpers".to_string())
        );
    }

    // --- removed-name diff ---

    #[test]
    fn removed_name_fires_and_reexport_keeps_quiet() {
        let before = "def heal():\n    pass\n\nclass Warden:\n    pass\n";
        // A re-export names `heal` whole-word, so it stays; `Warden` is gone.
        let after = "from x import heal\n";
        assert_eq!(removed_top_level_names(before, after), vec!["Warden"]);
    }

    #[test]
    fn rename_drops_only_the_old_name() {
        let before = "def heal(a):\n    return a\n";
        let after = "def heal2(a):\n    return a\n";
        assert_eq!(removed_top_level_names(before, after), vec!["heal"]);
    }

    #[test]
    fn substring_is_not_whole_word() {
        let before = "def heal():\n    pass\n";
        let after = "def healer():\n    pass\n";
        assert_eq!(removed_top_level_names(before, after), vec!["heal"]);
    }

    #[test]
    fn async_def_counts_as_top_level() {
        let before = "async def drain():\n    pass\n";
        assert_eq!(removed_top_level_names(before, ""), vec!["drain"]);
    }

    // --- test counters ---

    #[test]
    fn py_counter_counts_plain_and_async() {
        let text = "def test_a():\n    pass\n\nasync def test_b():\n    pass\n\ndef helper():\n    pass\n    def test_nested(): pass\n";
        assert_eq!(count_py_tests(text), 3);
    }

    #[test]
    fn rs_counter_counts_plain_and_paths() {
        let text =
            "#[test]\nfn a() {}\n\n#[tokio::test]\nasync fn b() {}\n\n#[cfg(test)]\nmod m {}\n";
        assert_eq!(count_rs_tests(text), 2);
    }

    // --- last-line verdicts ---

    #[test]
    fn cut_trailing_newline_names_the_baseline() {
        let b = baseline(&lines(3), "HEAD");
        let finding = last_line_finding(Some(&b), "a\nb").expect("fires");
        assert!(
            finding.contains("last line has no newline and HEAD did"),
            "{finding}"
        );
    }

    #[test]
    fn new_file_unterminated_advises_once() {
        let finding = last_line_finding(None, "new").expect("fires");
        assert!(finding.contains("last line has no newline;"), "{finding}");
    }

    #[test]
    fn baseline_unterminated_stays_quiet() {
        let b = baseline("no newline", "HEAD");
        assert!(last_line_finding(Some(&b), "also no").is_none());
    }

    #[test]
    fn emptied_file_reports_the_old_count() {
        let b = baseline(&lines(7), "before this edit");
        let finding = last_line_finding(Some(&b), "").expect("fires");
        assert_eq!(finding, "file is now empty; before this edit had 7 lines.");
    }

    // --- test-count verdict ---

    #[test]
    fn count_falls_against_head() {
        let b = baseline(
            "def test_a():\n    pass\ndef test_b():\n    pass\ndef test_c():\n    pass\n",
            "HEAD",
        );
        let after = "def test_a():\n    pass\ndef test_b():\n    pass\n";
        let finding = test_count_finding("py", Some(&b), after).expect("fires");
        assert!(
            finding.contains("test count fell 3 -> 2 against HEAD"),
            "{finding}"
        );
        assert!(finding.contains("AGENTS.md"), "{finding}");
    }

    #[test]
    fn count_rise_or_equal_stays_quiet() {
        let b = baseline("def test_a():\n    pass\n", "HEAD");
        let after = "def test_a():\n    pass\ndef test_b():\n    pass\n";
        assert!(test_count_finding("py", Some(&b), after).is_none());
    }

    // --- parse gate ---

    #[test]
    fn new_invalid_file_under_fixtures_is_skipped() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("fixtures").join("broken.py");
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        std::fs::write(&path, "def (").expect("write");
        assert!(parse_finding("def (", "py", &path, None).is_none());
    }

    #[test]
    fn new_invalid_file_outside_fixtures_fires() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("broken.py");
        std::fs::write(&path, "def (").expect("write");
        let finding = parse_finding("def (", "py", &path, None).expect("fires");
        assert!(finding.contains("SyntaxError"), "{finding}");
    }

    #[test]
    fn baseline_already_broken_is_skipped() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("broken.py");
        std::fs::write(&path, "def (").expect("write");
        let b = baseline("def (", "HEAD");
        assert!(parse_finding("def (", "py", &path, Some(&b)).is_none());
    }

    #[test]
    fn broken_json_against_parsing_head_fires() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("cfg.json");
        std::fs::write(&path, "{").expect("write");
        let b = baseline("{}\n", "HEAD");
        let finding = parse_finding("{", "json", &path, Some(&b)).expect("fires");
        assert!(!finding.is_empty(), "{finding}");
    }

    // --- stale-hit filter ---

    fn hit(file: &str, lineno: usize, line: &str) -> String {
        format!("{file}:{lineno}:{line}")
    }

    fn stale_filter(dotted: &str, name: &str, hits: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        for hit_line in hits {
            let Some((file, lineno, line)) = split_grep_hit(hit_line) else {
                continue;
            };
            if let Some((f, l, text)) =
                stale_hit_at(file, lineno, line, dotted, name, Path::new(""))
            {
                out.push(format!(
                    "removed top-level {name}; still named at {f}:{l}: {text}"
                ));
            }
        }
        out
    }

    #[test]
    fn dotted_string_hit_counts() {
        let out = stale_filter(
            "fno.demo.helpers",
            "heal",
            &[hit(
                "cli/tests/test_x.py",
                7,
                "    patch(\"fno.demo.helpers.heal\")",
            )],
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("test_x.py:7"), "{}", out[0]);
    }

    #[test]
    fn one_line_import_hit_counts() {
        let out = stale_filter(
            "fno.demo.helpers",
            "heal",
            &[hit(
                "cli/tests/test_x.py",
                3,
                "from fno.demo.helpers import heal",
            )],
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn import_of_other_name_stays_quiet() {
        let out = stale_filter(
            "fno.demo.helpers",
            "heal",
            &[hit(
                "cli/tests/test_x.py",
                3,
                "from fno.demo.helpers import other",
            )],
        );
        assert!(out.is_empty());
    }

    #[test]
    fn longer_module_sharing_prefix_stays_quiet() {
        let out = stale_filter(
            "fno.demo.helpers",
            "heal",
            &[
                hit(
                    "cli/tests/test_x.py",
                    3,
                    "from fno.demo.helpers_extra import heal",
                ),
                hit(
                    "cli/tests/test_x.py",
                    4,
                    "patch(\"fno.demo.helpers_extra.heal\")",
                ),
            ],
        );
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn parenthesized_import_names_the_body_line() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let test_file = dir.path().join("test_x.py");
        std::fs::write(
            &test_file,
            "from fno.demo.helpers import (\n    heal,\n    other,\n)\n",
        )
        .expect("write");
        let found = stale_hit_at(
            test_file.file_name().unwrap().to_str().unwrap(),
            1,
            "from fno.demo.helpers import (",
            "fno.demo.helpers",
            "heal",
            dir.path(),
        )
        .expect("parenthesized body names heal");
        assert_eq!(found.1, 2, "names line 2, the body line");
        assert!(found.2.contains("heal,"), "{}", found.2);
    }

    // --- end to end over a throwaway git repo ---

    /// AC2 shape: a committed module loses top-level `heal` while a test
    /// still patches the dotted string. One run names the patch line.
    #[test]
    fn end_to_end_names_the_stale_patch_target() {
        let repo = tempfile::TempDir::new().expect("tempdir");
        let root = repo.path();
        git(root, &["init", "-q"]);
        let module = root.join("helpers.py");
        std::fs::write(&module, "def heal():\n    pass\n\ndef keep():\n    pass\n").expect("write");
        let test = root.join("test_helpers.py");
        std::fs::write(
            &test,
            "from fno import helpers\n\ndef test_one():\n    patch(\"helpers.heal\")\n\ndef test_two():\n    pass\n",
        )
        .expect("write");
        git(root, &["add", "."]);
        git(
            root,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "base",
            ],
        );
        std::fs::write(&module, "def keep():\n    pass\n").expect("rewrite");

        let findings = check_paths(&[module.clone()], "HEAD", None);
        let stale: Vec<&String> = findings
            .iter()
            .filter(|f| f.contains("removed top-level heal"))
            .collect();
        assert_eq!(stale.len(), 1, "{findings:?}");
        assert!(stale[0].contains("test_helpers.py:4"), "{}", stale[0]);
        assert!(is_blocking(stale[0]));
    }

    /// AC1 shape through the checks: a tracked test file cut from 3 tests
    /// to 2 mid-line reports the drop against HEAD; advice does not block.
    #[test]
    fn end_to_end_test_count_drop_against_head() {
        let repo = tempfile::TempDir::new().expect("tempdir");
        let root = repo.path();
        git(root, &["init", "-q"]);
        let test = root.join("test_x.py");
        std::fs::write(
            &test,
            "def test_a():\n    pass\ndef test_b():\n    pass\ndef test_c():\n    pass\n",
        )
        .expect("write");
        git(root, &["add", "."]);
        git(
            root,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "base",
            ],
        );
        std::fs::write(&test, "def test_a():\n    pass\ndef test_b():\n    pass").expect("rewrite");

        let findings = check_paths(&[test.clone()], "HEAD", None);
        let count: Vec<&String> = findings
            .iter()
            .filter(|f| f.contains("test count fell 3 -> 2 against HEAD"))
            .collect();
        assert_eq!(count.len(), 1, "{findings:?}");
        assert!(!findings.iter().any(|f| is_blocking(f)), "{findings:?}");
    }

    /// AC10 shape: with a `--before` text, an uncommitted file's intended
    /// drop is measured against the pre-edit text, and the message says so.
    #[test]
    fn end_to_end_before_baseline_names_itself() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let test = dir.path().join("test_u.py");
        std::fs::write(
            &test,
            "def test_a():\n    pass\ndef test_b():\n    pass\ndef test_c():\n    pass\n",
        )
        .expect("write");
        let before = dir.path().join("before.py");
        std::fs::write(
            &before,
            "def test_a():\n    pass\ndef test_b():\n    pass\ndef test_c():\n    pass\n",
        )
        .expect("write");
        std::fs::write(&test, "def test_a():\n    pass\ndef test_b():\n    pass").expect("rewrite");

        let findings = check_paths(&[test.clone()], "HEAD", Some(&before));
        let count: Vec<&String> = findings
            .iter()
            .filter(|f| f.contains("test count fell 3 -> 2 against before this edit"))
            .collect();
        assert_eq!(count.len(), 1, "{findings:?}");
    }

    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
