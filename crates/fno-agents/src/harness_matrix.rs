//! The matrix renderer: ONE implementation of rendering both harness
//! matrices from the capability table's measurements. It ports and deletes
//! `scripts/diagnostics/render-harness-matrix.py`, which copied the table's
//! declared state words into the published cells; the renderer here prints a
//! state only where the table can point at a measurement.

use crate::harness_capabilities::{HarnessContract, JOURNEY_KEYS};
use crate::harness_reader::wait_for_ci_cell;
use std::path::{Path, PathBuf};

/// The phrase an unprobeable reason carries when the declared reader was
/// refused as blind and converted. The renderer's second unmeasured reason
/// reads the table's own words, never re-derives them.
pub const BLIND_REFUSAL_MARKER: &str = "reader refused as blind";

const MATRIX_REL: &str = "docs/harnesses/capability-matrix.md";
const VERB_MATRIX_REL: &str = "docs/harnesses/verb-matrix.md";

/// The full harness roster, read from the platform data file the
/// roster-parity gate already holds equal across the evidence surfaces.
/// Pure data, so the renderer reads it rather than keeping a second roster
/// in Rust.
fn known_harnesses(root: &Path) -> Result<Vec<String>, String> {
    let path = root.join("cli/src/fno/harness_names.py");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let start = text
        .find("KNOWN_HARNESSES")
        .ok_or_else(|| "the roster file names no KNOWN_HARNESSES tuple".to_string())?;
    let open = text[start..]
        .find('(')
        .map(|i| start + i)
        .ok_or_else(|| "KNOWN_HARNESSES carries no open paren".to_string())?;
    let close = text[open..]
        .find(')')
        .map(|i| open + i)
        .ok_or_else(|| "KNOWN_HARNESSES tuple never closes".to_string())?;
    let body = &text[open + 1..close];
    Ok(body
        .split(',')
        .map(|part| {
            part.trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .to_string()
        })
        .filter(|name| !name.is_empty())
        .collect())
}

/// Resolve the repo root from the caller's cwd: the nearest ancestor that
/// holds the canonical capability table.
fn resolve_root(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(root) = explicit {
        return Ok(root.to_path_buf());
    }
    let mut dir = std::env::current_dir().map_err(|e| format!("cannot read the cwd: {e}"))?;
    loop {
        if dir
            .join("crates/fno-agents/src/harness_capabilities.toml")
            .is_file()
        {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(
                "cannot locate the repo root: no ancestor holds the canonical capability table"
                    .to_string(),
            );
        }
    }
}

/// One features cell. With a `measured_by` receipt the cell prints the state
/// and the date it was settled (naming the version it was measured on when
/// the receipt carries one); without one it prints `unmeasured` with one of
/// three distinct reasons derived from the table itself.
pub fn features_cell(
    decl: Option<&crate::harness_capabilities::ProbeDecl>,
    claim: Option<&crate::harness_capabilities::FeatureClaim>,
) -> String {
    let Some(claim) = claim else {
        return "`unmeasured` (no reader declared)".to_string();
    };
    if let Some(m) = &claim.measured_by {
        return match m.version.is_empty() {
            true => format!("`{}` ({})", claim.state, m.date),
            false => format!("`{}` ({}, {})", claim.state, m.version, m.date),
        };
    }
    let reason = match decl {
        None => "no reader declared".to_string(),
        Some(decl) if decl.kind == "unprobeable" => {
            if decl.reason.contains(BLIND_REFUSAL_MARKER) {
                "reader refused as blind".to_string()
            } else {
                "no reader declared".to_string()
            }
        }
        Some(_) => "reader declared but not run".to_string(),
    };
    format!("`unmeasured` ({reason})")
}

/// The per-harness denominator: measured over declared as two numbers, a
/// percentage only when both sides are present, and a distinct sentence for
/// an empty declared set.
pub fn denominator_line(harness: &str, measured: usize, declared: usize) -> String {
    if declared == 0 {
        return format!("- {harness}: no feature keys are declared, so nothing can be measured");
    }
    if measured == 0 {
        return format!("- {harness}: 0 of {declared} measured");
    }
    let pct = measured * 100 / declared;
    format!("- {harness}: {measured} of {declared} measured ({pct}%)")
}

/// The journeys section: one row per journey class, one cell per harness.
/// Cells read unmeasured (the live tier is unearned everywhere); the
/// wait-for-CI row is the exception, because its measurement already exists
/// as code - the watch lease.
fn journeys_section(contract: &HarnessContract, roster: &[String]) -> String {
    let mut out = String::new();
    out.push_str("\n## Journey classes\n\n");
    out.push_str("The ten user-journey classes the cross-harness audits name, one row per class, one cell per harness. A cell reads unmeasured until a live-tier run earns it; nobody looked is not measured absence. The wait-for-CI row is settled by code, not by a table word: the watch lease permits only claude to idle, and every other harness reads absent with its refusal quoted.\n\n");
    out.push_str("| journey | instrument | ");
    out.push_str(&roster.join(" | "));
    out.push_str(" |\n|");
    for _ in 0..roster.len() + 2 {
        out.push_str("---|");
    }
    out.push('\n');
    for key in JOURNEY_KEYS {
        let decl = contract.journeys.get(key);
        let instrument = match decl {
            Some(decl) if decl.kind == "unprobeable" => decl.reason.clone(),
            Some(decl) if !decl.marker.is_empty() => {
                let reader = if decl.reader.is_empty() {
                    "the live rubric"
                } else {
                    decl.reader.as_str()
                };
                format!("runner: {reader} (live tier)")
            }
            Some(decl) => {
                if decl.reader.is_empty() {
                    "the live rubric".to_string()
                } else {
                    decl.reader.clone()
                }
            }
            _ => "no declaration".to_string(),
        };
        out.push_str(&format!("| {key} | {instrument} |"));
        for harness in roster {
            let cell = if key == "wait-for-ci" {
                let (state, refusal) = wait_for_ci_cell(harness);
                if refusal.is_empty() {
                    format!("`{state}`")
                } else {
                    format!("`{state}` {refusal}")
                }
            } else {
                "`unmeasured`".to_string()
            };
            out.push_str(&format!(" {cell} |"));
        }
        out.push('\n');
    }
    out
}

/// The features matrix: the generated-copy header, the state vocabulary, the
/// features grid, the journey classes, and the measured-over-declared
/// denominator lines.
pub fn render_matrix(contract: &HarnessContract, roster: &[String]) -> String {
    let mut out = String::new();
    out.push_str("<!-- GENERATED by `fno doctor harness-matrix --write`. DO NOT EDIT.\n");
    out.push_str("     A hand edit trips the rust-ci generated-copies dirty-tree step;\n");
    out.push_str("     regenerate from crates/fno-agents/src/harness_capabilities.toml. -->\n");
    out.push_str("# Harness capability matrix (features)\n\n");
    let blind = contract
        .probe
        .iter()
        .filter(|(field, decl)| {
            field.starts_with("features.")
                && decl.kind == "unprobeable"
                && decl.reason.contains(BLIND_REFUSAL_MARKER)
        })
        .count();
    out.push_str(&format!(
        "What each harness can DO, rendered from the capability table (map_version {}). One row per supported harness, including roster entries with no capability row, which read unmeasured everywhere, and one column per feature key. The table carries {blind} feature-key instruments that were refused as blind and converted to honest unprobeables: their word-match patterns matched a sentence denying the capability as readily as support, so the reader could not report absence, and a state they settled renders unmeasured with the reason reader refused as blind.\n\n",
        contract.map_version
    ));
    out.push_str("| State | Meaning |\n|---|---|\n");
    out.push_str("| `native` | fno drives it today through a wired lane: the harness's own surface where it exposes one, else a lane fno hosts (the daemon-kept portal) |\n");
    out.push_str("| `capable` | real on the harness, and fno has no wired arm |\n");
    out.push_str("| `absent` | measured not to exist on this harness |\n");
    out.push_str("| `unmeasured` | nobody has looked |\n\n");
    out.push_str("Every non-native cell settles the same way: a live-tier run of the declared instrument earns the cell, and until then it reads unmeasured with its reason. The pane-driving mechanics are a different dimension and live in the capability table itself, never here.\n\n");
    let keys: Vec<&str> = contract
        .probe
        .keys()
        .filter(|f| f.starts_with("features."))
        .map(|f| &f["features.".len()..])
        .collect();
    out.push_str("| harness | ");
    out.push_str(&keys.join(" | "));
    out.push_str(" |\n|");
    for _ in 0..keys.len() + 1 {
        out.push_str("---|");
    }
    out.push('\n');
    for name in roster {
        let cells: Vec<String> = keys
            .iter()
            .map(|key| {
                let decl = contract.probe.get(&format!("features.{key}"));
                let claim = contract
                    .capabilities(name)
                    .ok()
                    .and_then(|caps| caps.features.get(*key));
                features_cell(decl, claim)
            })
            .collect();
        out.push_str(&format!("| {name} | "));
        out.push_str(&cells.join(" | "));
        out.push_str(" |\n");
    }
    out.push('\n');
    out.push_str(&journeys_section(contract, roster));
    out.push('\n');
    out.push_str("## Measured over declared\n\n");
    for name in roster {
        let measured = contract
            .capabilities(name)
            .map(|caps| {
                caps.features
                    .values()
                    .filter(|claim| claim.measured_by.is_some())
                    .count()
            })
            .unwrap_or(0);
        let declared = contract
            .probe
            .keys()
            .filter(|f| f.starts_with("features."))
            .count();
        out.push_str(&denominator_line(name, measured, declared));
        out.push('\n');
    }
    out.push('\n');
    out.push_str("## Readers (row legend)\n\n");
    for name in roster {
        let measured_claims: Vec<String> = contract
            .capabilities(name)
            .ok()
            .and_then(|caps| {
                let mut rows: Vec<String> = caps
                    .features
                    .iter()
                    .filter(|(_, claim)| claim.measured_by.is_some())
                    .map(|(key, claim)| {
                        let m = claim.measured_by.as_ref().expect("filtered");
                        match m.version.is_empty() {
                            true => format!("{key} by {} ({})", m.reader, m.date),
                            false => format!("{key} by {} ({}, {})", m.reader, m.version, m.date),
                        }
                    })
                    .collect();
                rows.sort();
                Some(rows)
            })
            .unwrap_or_default();
        if measured_claims.is_empty() {
            out.push_str(&format!(
                "- {name}: no cell carries a measurement receipt\n"
            ));
        } else {
            out.push_str(&format!("- {name}: {}\n", measured_claims.join("; ")));
        }
    }
    out
}

/// A skill's declared harness needs, read off its frontmatter:
/// `metadata.requires.harness`. A value outside the vocabulary refuses: a
/// typo that silently read as no needs would render native by default.
fn skill_needs(
    skills_dir: &Path,
    vocabulary: &[String],
) -> Result<Vec<(String, Vec<String>)>, String> {
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(skills_dir)
        .map_err(|e| format!("cannot read the skills directory: {e}"))?
        .filter_map(|entry| entry.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    let mut needs = Vec::new();
    for entry in entries {
        let path = entry.path().join("SKILL.md");
        if !path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let name = frontmatter_name(&text)
            .unwrap_or_else(|| entry.file_name().to_string_lossy().to_string());
        let declared = frontmatter_harness_needs(&text)?;
        for value in &declared {
            if !vocabulary.contains(value) {
                return Err(format!(
                    "{}: metadata.requires.harness value {value:?} is outside the vocabulary",
                    path.display()
                ));
            }
        }
        needs.push((name, declared));
    }
    Ok(needs)
}

/// The skill's `name:` from its frontmatter, when present.
fn frontmatter_name(text: &str) -> Option<String> {
    let fm = frontmatter(text)?;
    for line in fm.lines() {
        if let Some(rest) = line.strip_prefix("name:") {
            return Some(rest.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// The frontmatter block between the leading `---` lines.
fn frontmatter(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

/// The `metadata.requires.harness` list from a skill's frontmatter.
fn frontmatter_harness_needs(text: &str) -> Result<Vec<String>, String> {
    let fm = frontmatter(text).ok_or_else(|| "no frontmatter".to_string())?;
    let mut in_metadata = false;
    let mut in_requires = false;
    let mut in_harness = false;
    let mut needs = Vec::new();
    for line in fm.lines() {
        if line.starts_with("metadata:") {
            in_metadata = true;
            continue;
        }
        if line.starts_with("requires:") && in_metadata {
            in_requires = true;
            continue;
        }
        if line.trim_start().starts_with("harness:") && in_metadata && in_requires {
            in_harness = true;
            continue;
        }
        if in_harness {
            let trimmed = line.trim_start();
            if trimmed.starts_with("- ") {
                needs.push(trimmed[2..].trim().to_string());
                continue;
            }
            in_metadata = false;
            in_requires = false;
            in_harness = false;
            // A blank or lesser-indented line ends the list; fall through.
            if line.trim().is_empty() {
                continue;
            }
        }
        // A non-list line resets the nesting state cleanly.
        if !line.starts_with(' ') && !line.trim().is_empty() {
            in_metadata = false;
            in_requires = false;
            in_harness = false;
        }
    }
    Ok(needs)
}

/// The loop need's cell: `extension` with an EMPTY `loop_extension` reads
/// `absent`, quoting the refusal the dispatch gate gives, never native. A
/// harness whose loop extension is shipped (the control) the same reader
/// admits as native.
pub fn loop_cell(
    row: Option<&crate::harness_capabilities::HarnessCapabilities>,
) -> (String, Option<String>) {
    let Some(row) = row else {
        return ("unmeasured".to_string(), None);
    };
    match row.loop_participation.as_str() {
        "native" => ("native".to_string(), None),
        "extension" if row.loop_extension.is_empty() => (
            "absent".to_string(),
            Some(
                "refuses looping dispatch: loop_participation = \"extension\" with an empty loop_extension, so nothing invokes loop-check".to_string(),
            ),
        ),
        "extension" => ("native".to_string(), None),
        "none" => ("absent".to_string(), None),
        _ => ("unmeasured".to_string(), None),
    }
}

/// The verb matrix: one row per skill, one column per harness. A cell is a
/// projection, never a fresh measurement: no capability row reads
/// unmeasured, a refused command surface reads absent, and otherwise the
/// cell is the worst declared need, ordered absent < unmeasured < capable <
/// native.
fn render_verb_matrix(
    contract: &HarnessContract,
    roster: &[String],
    skills_dir: &Path,
) -> Result<String, String> {
    let vocabulary: Vec<String> = [
        vec!["loop".to_string()],
        contract
            .probe
            .keys()
            .filter(|f| f.starts_with("features."))
            .map(|f| f["features.".len()..].to_string())
            .collect::<Vec<String>>(),
        roster.to_vec(),
    ]
    .concat();
    let needs = skill_needs(skills_dir, &vocabulary)?;
    let mut out = String::new();
    out.push_str("<!-- GENERATED by `fno doctor harness-matrix --write`. DO NOT EDIT.\n");
    out.push_str("     A hand edit trips the guards verb-matrix freshness step;\n");
    out.push_str("     regenerate from crates/fno-agents/src/harness_capabilities.toml\n");
    out.push_str("     and skills/*/SKILL.md frontmatter. -->\n");
    out.push_str("# Verb x harness matrix\n\n");
    out.push_str(&format!(
        "Which fno verb runs on which harness, rendered from the capability table (map_version {}) and each skill's `metadata.requires.harness` frontmatter. One row per skills/*/SKILL.md, one column per supported harness. The states are the features vocabulary in [capability-matrix.md](capability-matrix.md).\n\n",
        contract.map_version
    ));
    out.push_str("A cell is a projection, never a fresh measurement. The rule, in order:\n\n");
    out.push_str("1. No capability row for the harness: `unmeasured`.\n");
    out.push_str("2. `command_surface = \"refused\"`: `absent`. Dispatch refuses to render any verb there.\n");
    out.push_str("3. Otherwise start at `native` and take the worst declared need, ordered absent < unmeasured < capable < native. The loop need reads the refusal: loop_participation `extension` with an empty loop_extension renders `absent` with the refusal quoted below, because dispatch refuses to loop there - measured absence, not capable.\n\n");
    out.push_str("| verb | needs | ");
    out.push_str(&roster.join(" | "));
    out.push_str(" |\n|");
    for _ in 0..roster.len() + 2 {
        out.push_str("---|");
    }
    out.push('\n');
    let mut loop_refusals: Vec<String> = Vec::new();
    for (name, needs) in &needs {
        let cells: Vec<String> = roster
            .iter()
            .map(|harness| {
                let row = contract.capabilities(harness).ok();
                if row.is_none() {
                    return "`unmeasured`".to_string();
                }
                let row = row.expect("checked");
                if row.command_surface == "refused" {
                    return "`absent`".to_string();
                }
                let mut state = 3usize; // native
                let order = |s: &str| match s {
                    "absent" => 0usize,
                    "unmeasured" => 1,
                    "capable" => 2,
                    _ => 3,
                };
                for need in needs {
                    let need_state = if need == "loop" {
                        let (state, refusal) = loop_cell(Some(row));
                        if let Some(refusal) = refusal {
                            let line = format!("- {harness}: {refusal}");
                            if !loop_refusals.contains(&line) {
                                loop_refusals.push(line);
                            }
                        }
                        state
                    } else if roster.contains(need) {
                        if need == harness {
                            "native".to_string()
                        } else {
                            "absent".to_string()
                        }
                    } else {
                        let decl = contract.probe.get(&format!("features.{need}"));
                        let claim = row.features.get(need.as_str());
                        features_cell(decl, claim)
                    };
                    if order(&need_state) < state {
                        // The published cell shows the worst need; a feature
                        // cell that carries its reason text (unmeasured ...)
                        // flattens to the state word for the projection.
                        let base = cell_state_word(&need_state);
                        state = order(&base);
                    }
                }
                ["absent", "unmeasured", "capable", "native"][state].to_string()
            })
            .collect();
        let needs_col = if needs.is_empty() {
            "-".to_string()
        } else {
            needs.join(", ")
        };
        out.push_str(&format!("| {name} | {needs_col} | "));
        out.push_str(&cells.join(" | "));
        out.push_str(" |\n");
    }
    out.push('\n');
    if !loop_refusals.is_empty() {
        out.push_str("The loop refusals the matrix quotes:\n\n");
        for line in loop_refusals {
            out.push_str(&line);
            out.push('\n');
        }
    }
    Ok(out)
}

/// The transport-only door: `harness-matrix [--write] [--table <path>]
/// [--root <path>]`. Caller: the `fno doctor harness-matrix` leaf and the
/// guards freshness step.
pub fn run_client(args: &[String]) -> i32 {
    let write = args.iter().any(|a| a == "--write");
    let table = flag_value(args, "--table");
    let root_flag = flag_value(args, "--root");
    let root = match resolve_root(root_flag.as_deref().map(Path::new)) {
        Ok(root) => root,
        Err(e) => {
            eprintln!("fno doctor harness-matrix: {e}");
            return 2;
        }
    };
    let text = match std::fs::read_to_string(
        table
            .as_deref()
            .map(Path::new)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.join("crates/fno-agents/src/harness_capabilities.toml")),
    ) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("fno doctor harness-matrix: cannot read the capability table: {e}");
            return 2;
        }
    };
    let contract = match HarnessContract::parse(&text) {
        Ok(contract) => contract,
        Err(e) => {
            eprintln!("fno doctor harness-matrix: capability contract error: {e}");
            return 2;
        }
    };
    let roster = match known_harnesses(&root) {
        Ok(roster) => roster,
        Err(e) => {
            eprintln!("fno doctor harness-matrix: {e}");
            return 2;
        }
    };
    let matrix = render_matrix(&contract, &roster);
    let verb_matrix = match render_verb_matrix(&contract, &roster, &root.join("skills")) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("fno doctor harness-matrix: {e}");
            return 2;
        }
    };
    if write {
        for (rel, text) in [(MATRIX_REL, &matrix), (VERB_MATRIX_REL, &verb_matrix)] {
            let target = root.join(rel);
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&target, text) {
                eprintln!("fno doctor harness-matrix: cannot write {rel}: {e}");
                return 2;
            }
            println!("harness matrix written to {rel}");
        }
        0
    } else {
        println!("{matrix}");
        println!();
        println!("{verb_matrix}");
        0
    }
}

/// A `--flag value` reader for the door.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// The verb-matrix row builder's state flattening helper, split out so the
/// closure stays readable. A features cell carries its reason text in
/// parentheses; the projection needs the bare state word.
fn cell_state_word(cell: &str) -> String {
    cell.split('`').nth(1).unwrap_or("unmeasured").to_string()
}
