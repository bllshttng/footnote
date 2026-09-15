//! Source guard for the machine-output contract
//! (docs/architecture/json-output-contract.md).
//!
//! Every hand parser under `crates/` that names `--json` must accept the one
//! machine-output request on both spellings: the line either spells `"-J"`
//! beside `"--json"` or routes through `json_output::`. A parse line that
//! names only `--json` splits the contract again - `fno-agents version -J`
//! printing human text at exit 0 was the worst shape that took.

use regex::Regex;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The parse sweep from the plan's `surface:` block. `--json` in a flag
/// loop, in an arm, or inside an `.any()` scan: the shapes a hand parser
/// actually writes. Cosmetic references (help text, usage lines, argv lists)
/// do not match, because none of them end in `=>` or `if `.
fn sweep() -> Regex {
    Regex::new(
        r#""--json"\s*(\|\s*"-J"\s*)?(=>|if )|Some\("--json"\)\s*=>|has_flag\(args, "--json"\)|a == "--json"|s == "--json"|"-J"\s*(\|\s*"--json"\s*)?=>"#,
    )
    .expect("sweep regex")
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn every_parse_line_accepts_both_spellings() {
    let re = sweep();
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let crates_src = manifest_dir
        .parent()
        .expect("crates dir")
        .join("fno")
        .join("src");

    let mut files = Vec::new();
    collect_rs(&manifest_dir.join("src"), &mut files);
    collect_rs(&crates_src, &mut files);
    assert!(
        files.len() > 100,
        "the scan found only {} source files; the walk is broken",
        files.len()
    );

    let mut violations: Vec<String> = Vec::new();
    let mut matched_files: BTreeSet<String> = BTreeSet::new();
    let mut total_matches = 0usize;
    for file in &files {
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for (n, line) in text.lines().enumerate() {
            if !re.is_match(line) {
                continue;
            }
            total_matches += 1;
            let shown = file.display().to_string();
            matched_files.insert(shown.clone());
            let conforming = line.contains("\"-J\"") || line.contains("json_output::");
            if !conforming {
                violations.push(format!(
                    "{}:{}: {}\n    use json_output::is_flag / requested, or spell both spellings",
                    shown,
                    n + 1,
                    line.trim_start()
                ));
            }
        }
    }

    assert!(
        total_matches > 0,
        "the sweep matched zero parse lines; the regex no longer sees the parsers it guards"
    );
    assert!(
        matched_files.iter().any(|f| f.ends_with("needs.rs")),
        "needs.rs (the conforming reference) is not among the scanned files: {matched_files:?}"
    );
    assert!(
        violations.is_empty(),
        "{} parse line(s) name --json without -J or json_output:::\n{}",
        violations.len(),
        violations.join("\n")
    );
}
