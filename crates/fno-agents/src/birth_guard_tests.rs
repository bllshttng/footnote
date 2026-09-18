//! Rust birth guard: every `agent_spawned` emit in production Rust builds
//! its payload through `spawn_edge::birth_event`, the one builder that
//! stamps the lineage triple or the reason a birth could not name a parent
//! (the rule `registry_schema.toml` states in both trees and
//! `registry.py::mint_agent_entry` enforces on the Python leg).
//!
//! A line scan, deliberately not the mint guard's brace-tracking lexer: the
//! emit kind is a plain string literal, so a site is a line carrying
//! `"agent_spawned"` within a few lines of an `emit(` call, and the same
//! statement passes when a later line in the call carries `birth_event(`.
//! Two named ceilings, both fail visible rather than silent: an emit whose
//! builder call sits more than a dozen lines below the kind line would be
//! flagged (never waved through), and kind lists or reader arms (`Some(
//! "agent_spawned") =>`, `KNOWN_EVENT_KINDS`) are not sites because nothing
//! near them calls emit. Test code is skipped by path (`/tests/`,
//! `*_tests.rs`) and by rustfmt's `#[cfg(test)]` mod shape, whose closing
//! brace is the only col-0 `}` inside the mod.

use std::fs;
use std::path::Path;

const KIND: &str = "\"agent_spawned\"";
const BUILDER: &str = "birth_event(";

fn is_test_path(rel: &str) -> bool {
    rel.contains("/tests/") || rel.ends_with("_tests.rs")
}

/// The builder's home: it names the kind in its doc comment by design.
fn is_builder_home(rel: &str) -> bool {
    rel == "spawn_edge.rs"
}

/// One pass over (rel, lines) pairs. Returns offender "rel:line" strings and
/// the count of emit sites matched, so the caller can tell an honest pass
/// from a scan that never saw production code.
fn scan(sources: &[(String, Vec<String>)]) -> (Vec<String>, usize) {
    let mut offenders = Vec::new();
    let mut sites = 0;
    for (rel, lines) in sources {
        if is_test_path(rel) || is_builder_home(rel) {
            continue;
        }
        let mut in_test = false;
        for (i, line) in lines.iter().enumerate() {
            if line.trim_start().starts_with("#[cfg(test)]") {
                in_test = true;
                continue;
            }
            if in_test {
                // rustfmt keeps a mod's own close at column 0; every inner
                // item's brace is indented. Match the column, never the trim.
                if line == "}" {
                    in_test = false;
                }
                continue;
            }
            if !line.contains(KIND) {
                continue;
            }
            let near_emit = lines[i.saturating_sub(6)..=i]
                .iter()
                .any(|l| is_emit_line(l));
            if !near_emit {
                continue;
            }
            sites += 1;
            // The builder call sits where the door put it: below the kind
            // line in the emitter shape, above it in opencode_serve, where
            // the payload is built and then converted to pairs (18 lines).
            // A line scan cannot parse statements, so the window is wide in
            // both directions; the cost is that a birth_event( in a
            // NEIGHBORING statement can cover a nearby bare emit - fail
            // visible would only be lost to a second bare emit within a few
            // lines.
            let start = i.saturating_sub(20);
            let end = (i + 13).min(lines.len());
            let covered = lines[start..end].iter().any(|l| l.contains(BUILDER));
            if !covered {
                let snippet = line.trim().to_string();
                offenders.push(format!("{rel}:{}: {snippet}", i + 1));
            }
        }
    }
    (offenders, sites)
}

fn collect(dir: &Path, root: &Path, out: &mut Vec<(String, Vec<String>)>) {
    let mut entries: Vec<_> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, root, out);
        } else if path.extension().map_or(false, |e| e == "rs") {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if let Ok(text) = fs::read_to_string(&path) {
                out.push((rel, text.lines().map(String::from).collect()));
            }
        }
    }
}

fn scan_real_tree() -> (Vec<String>, usize, usize) {
    // The workspace root, so the scan covers every crate's sources the way
    // the journal's readers do: crates/fno-agents -> crates -> root.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let crates = root.join("crates");
    let mut sources = Vec::new();
    collect(&crates, &crates, &mut sources);
    let files = sources.len();
    let (offenders, sites) = scan(&sources);
    (offenders, sites, files)
}

#[test]
fn scanner_flags_an_emit_without_the_builder() {
    let fixture = "fn f() {\n    let _ = ctx.emitter.emit(\n        \"agent_spawned\",\n        &json!({\"name\": name}),\n    );\n}\n";
    let (offenders, sites) = scan(&[("daemon.rs".into(), lines(fixture))]);
    assert_eq!(sites, 1);
    assert_eq!(offenders, vec!["daemon.rs:3: \"agent_spawned\","]);
}

#[test]
fn scanner_passes_an_emit_that_routes_through_the_builder() {
    let fixture = "fn f() {\n    let _ = ctx.emitter.emit(\n        \"agent_spawned\",\n        &crate::spawn_edge::birth_event(\n            name,\n            &crate::state::Lineage::from_request(&req.params),\n            json!({\"name\": name}),\n        ),\n    );\n}\n";
    let (offenders, sites) = scan(&[("daemon.rs".into(), lines(fixture))]);
    assert_eq!(sites, 1);
    assert_eq!(offenders, Vec::<String>::new());
}

#[test]
fn scanner_ignores_kind_lists_and_reader_arms() {
    let fixture = "const KNOWN: &[&str] = &[\n    \"agent_spawned\",\n    \"agent_removed\",\n];\nfn g(j: &Value) {\n    match j.get(\"type\") {\n        Some(\"agent_spawned\") => {}\n        _ => {}\n    }\n}\n";
    let (offenders, sites) = scan(&[("lib.rs".into(), lines(fixture))]);
    assert_eq!(sites, 0);
    assert_eq!(offenders, Vec::<String>::new());
}

#[test]
fn scanner_ignores_cfg_test_regions_and_whole_file_test_trees() {
    let fixture = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        em.emit(\"agent_spawned\", &json!({})).unwrap();\n    }\n}\nfn f() {\n    let _ = x.emit(\"agent_spawned\", &json!({}));\n}\n";
    let (offenders, sites) = scan(&[
        ("events.rs".into(), lines(fixture)),
        (
            "more_tests.rs".into(),
            lines("fn f() {\n    let _ = x.emit(\"agent_spawned\", &json!({}));\n}\n"),
        ),
    ]);
    // Only the production fn after the mod's col-0 close is a site.
    assert_eq!(sites, 1);
    assert_eq!(
        offenders,
        vec!["events.rs:9: let _ = x.emit(\"agent_spawned\", &json!({}));"]
    );
}

#[test]
fn the_builder_home_is_exempt_but_every_other_crates_file_is_scanned() {
    let (offenders, sites, files) = scan_real_tree();
    assert!(
        files > 20,
        "scan saw only {files} files; the walk is broken"
    );
    assert!(
        sites >= 4,
        "scanner matched only {sites} production emit sites; the four doors must all route through birth_event"
    );
    assert!(
        offenders.is_empty(),
        "agent_spawned emits bypassing spawn_edge::birth_event:\n{}",
        offenders.join("\n")
    );
}

/// An emit-kind site sits near an emit call: `.emit(` (the emitter shape
/// three doors share) or `emit_event(` (the pairs shape opencode_serve
/// takes). "emitter.emit(" contains the former; "emit_event(" matches only
/// the latter, so the two alternatives cover both shapes.
fn is_emit_line(l: &str) -> bool {
    l.contains("emit(") || l.contains("emit_event(")
}

fn lines(s: &str) -> Vec<String> {
    s.lines().map(String::from).collect()
}
