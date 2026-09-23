//! The doc-binding test for `docs/architecture/background-processes.md`.
//!
//! The page inventories every control-plane arm and every launchd label the
//! shipped source names. This test fails when the code grows an arm or a
//! label the page does not name, and when the page names a scheduler stamp
//! that differs from `KNOWN_ARMS`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

const DOC_RELATIVE: &str = "docs/architecture/background-processes.md";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_repo_file(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!("cannot read {relative} ({err}): {DOC_RELATIVE} must exist for this test")
    })
}

fn walk(dir: &Path, extension: &str, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, extension, out);
        } else if path.extension().is_some_and(|ext| ext == extension) {
            out.push(path);
        }
    }
}

/// A Rust file's text up to its inline test module: test fixtures name fake
/// launchd labels (`sh.fno.mux`, `sh.fno.idle`) no doc row may be asked for.
/// Item-level `#[cfg(test)]` attributes do not end the scan.
fn rust_source_minus_tests(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let mut kept = String::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.trim() == "#[cfg(test)]"
            && lines[index + 1..]
                .iter()
                .find(|candidate| !candidate.trim().is_empty())
                .is_some_and(|next| next.trim_start().starts_with("mod "))
        {
            break;
        }
        kept.push_str(line);
        kept.push('\n');
        index += 1;
    }
    kept
}

/// Every arm the code can emit: the `KNOWN_ARMS` table plus the Python tick
/// literals that have no row there (they are appended as observed-only rows).
fn code_arms(python_sources: &[PathBuf]) -> BTreeSet<String> {
    let mut arms: BTreeSet<String> = BTreeSet::new();
    for spec in fno_agents::tick_ledger::KNOWN_ARMS {
        arms.insert(spec.arm.to_string());
    }
    let patterns = [
        Regex::new(r#"_emit_tick_row\(\s*"([a-z_]+)""#).unwrap(),
        Regex::new(r#"\barm="([a-z_]+)""#).unwrap(),
        Regex::new(r#"\bemit_tick\(\s*"([a-z_]+)""#).unwrap(),
    ];
    for path in python_sources {
        let text = fs::read_to_string(path).unwrap_or_default();
        for pattern in &patterns {
            for capture in pattern.captures_iter(&text) {
                arms.insert(capture[1].to_string());
            }
        }
    }
    arms
}

/// Every `sh.fno.<label>` the shipped source names, Rust inline tests cut.
fn shipped_labels(python_sources: &[PathBuf], rust_sources: &[PathBuf]) -> BTreeSet<String> {
    let pattern = Regex::new(r"sh\.fno\.([a-z][a-z0-9-]*)").unwrap();
    let mut labels = BTreeSet::new();
    for path in python_sources {
        let text = fs::read_to_string(path).unwrap_or_default();
        for capture in pattern.captures_iter(&text) {
            labels.insert(capture[1].to_string());
        }
    }
    for path in rust_sources {
        let text = rust_source_minus_tests(path);
        for capture in pattern.captures_iter(&text) {
            labels.insert(capture[1].to_string());
        }
    }
    labels
}

fn section_after<'a>(doc: &'a str, heading: &str) -> Option<&'a str> {
    let marker = format!("## {heading}");
    let start = doc.find(&marker)? + marker.len();
    let rest = &doc[start..];
    let end = rest.find("\n## ").unwrap_or(rest.len());
    Some(&rest[..end])
}

fn first_backticked(cell: &str) -> Option<String> {
    let start = cell.find('`')?;
    let rest = &cell[start + 1..];
    let end = rest.find('`')?;
    Some(rest[..end].to_string())
}

/// One `(first cell, second cell)` pair per table row, backticks stripped.
/// The header and the `---` separator carry no backticked first cell, so they
/// drop out here; a table that yields zero rows is a failure, never a pass.
fn table_rows(section: &str) -> Vec<(String, String)> {
    section
        .lines()
        .filter(|line| line.trim_start().starts_with('|'))
        .filter_map(|line| {
            let cells: Vec<&str> = line.trim().trim_matches('|').split('|').collect();
            let first = first_backticked(cells.first()?)?;
            let second = cells.get(1).and_then(|cell| first_backticked(cell));
            Some((first, second.unwrap_or_default()))
        })
        .collect()
}

#[test]
fn the_background_processes_page_names_every_arm_and_shipped_label() {
    let doc = read_repo_file(DOC_RELATIVE);

    let mut python_sources = Vec::new();
    walk(&repo_root().join("cli/src/fno"), "py", &mut python_sources);
    let mut rust_sources = Vec::new();
    walk(&repo_root().join("crates/fno/src"), "rs", &mut rust_sources);
    walk(
        &repo_root().join("crates/fno-agents/src"),
        "rs",
        &mut rust_sources,
    );

    let arms = code_arms(&python_sources);
    let labels = shipped_labels(&python_sources, &rust_sources);
    assert!(
        !arms.is_empty(),
        "the arm scan found nothing; the walk roots are wrong"
    );
    assert!(
        !labels.is_empty(),
        "the label scan found nothing; the walk roots are wrong"
    );

    let arms_section = section_after(&doc, "Arms")
        .unwrap_or_else(|| panic!("{DOC_RELATIVE} has no `## Arms` section"));
    let launchd_section = section_after(&doc, "Launchd agents")
        .unwrap_or_else(|| panic!("{DOC_RELATIVE} has no `## Launchd agents` section"));

    let arm_rows = table_rows(arms_section);
    let doc_arms: BTreeSet<String> = arm_rows.iter().map(|(arm, _)| arm.clone()).collect();
    let launchd_rows = table_rows(launchd_section);
    let doc_labels: BTreeSet<String> = launchd_rows
        .iter()
        .map(|(label, _)| label.trim_start_matches("sh.fno.").to_string())
        .collect();

    let mut failures: Vec<String> = Vec::new();
    if arm_rows.is_empty() {
        failures.push("## Arms table has no rows".to_string());
    }
    if launchd_rows.is_empty() {
        failures.push("## Launchd agents table has no rows".to_string());
    }

    for arm in &arms {
        if !doc_arms.contains(arm) {
            failures.push(format!(
                "arm `{arm}` has no row in the {DOC_RELATIVE} arms table"
            ));
        }
    }
    for arm in &doc_arms {
        if !arms.contains(arm) {
            failures.push(format!(
                "{DOC_RELATIVE} arms table row `{arm}` matches no arm the code emits"
            ));
        }
    }
    for spec in fno_agents::tick_ledger::KNOWN_ARMS {
        let Some((_, scheduler)) = arm_rows.iter().find(|(arm, _)| arm == spec.arm) else {
            continue;
        };
        if scheduler != spec.scheduler {
            failures.push(format!(
                "{DOC_RELATIVE} arms table row `{}` names scheduler `{scheduler}` but KNOWN_ARMS stamps it `{}`",
                spec.arm, spec.scheduler
            ));
        }
    }
    for label in &labels {
        let named =
            doc_labels.contains(label) || launchd_section.contains(&format!("`sh.fno.{label}`"));
        if !named {
            failures.push(format!(
                "label `sh.fno.{label}` is named in shipped source but not in the {DOC_RELATIVE} launchd section"
            ));
        }
    }
    for label in &doc_labels {
        if !labels.contains(label) {
            failures.push(format!(
                "{DOC_RELATIVE} launchd table row `sh.fno.{label}` matches no label the shipped source names"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{DOC_RELATIVE} is out of step with the code:\n{}",
        failures.join("\n")
    );
}
