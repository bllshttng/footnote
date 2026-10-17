//! `fno backlog intake` - pull an existing plan file in as a backlog node.
//!
//! Ported from cli/src/fno/graph/cli.py (cmd_intake + _intake_impl) and
//! graph/_intake.py, with the depends machinery from graph/depends.py. A plan
//! is one delivery unit: plan == PR == node. The verb refuses a plan already
//! intaked (same roadmap or any), resolves claims (CLI flag strict,
//! frontmatter lenient), validates the difficulty gate, and mints or updates
//! through the shared graph write pipeline.

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// The graph's node-type vocabulary; the intake doc->graph flow validates
/// against this one set.
pub(crate) const VALID_NODE_TYPES: [&str; 4] = ["feature", "epic", "bug", "roadmap"];
const DEFAULT_NODE_TYPE: &str = "feature";

/// Coerce a plan-frontmatter size to the canonical S|M|L, else None. The
/// graph stores uppercase; anything outside the set is dropped, never stored.
pub(crate) fn normalize_size(value: Option<&str>) -> Option<String> {
    let s = value?.trim().to_uppercase();
    if s == "S" || s == "M" || s == "L" {
        Some(s)
    } else {
        None
    }
}

/// Coerce a plan-frontmatter type to the graph vocabulary, else the default.
/// Fail-soft and SILENT: plan frontmatter `type:` is an overloaded key (measured
/// over 607 real plans, 88 carry a document kind the graph does not share), so
/// a non-match simply is not a node type and leaves the default standing.
pub(crate) fn normalize_type(value: Option<&str>) -> String {
    match value {
        Some(t) => {
            let t = t.trim().to_lowercase();
            if VALID_NODE_TYPES.contains(&t.as_str()) {
                t
            } else {
                DEFAULT_NODE_TYPE.to_string()
            }
        }
        None => DEFAULT_NODE_TYPE.to_string(),
    }
}

/// The NN / NNa sequence token of a plan filename: `01-foo.md` -> `01`,
/// `02a-bar.md` -> `02a`. Resolves bare sequence references like
/// `depends_on: 02a` within a batch.
fn sequence_token(basename: &str) -> Option<String> {
    let bytes = basename.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && bytes[i].is_ascii_digit() && i < 2 {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    if i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'-' {
        Some(basename[..i].to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Full-YAML frontmatter reader (_read_plan_frontmatter / plan_claims)
// ---------------------------------------------------------------------------

fn strip_bom_first_line(line: &str) -> String {
    line.strip_prefix('\u{feff}')
        .unwrap_or(line)
        .trim()
        .to_string()
}

/// Parse YAML frontmatter from a single-file plan. Returns an empty map on
/// every failure mode (missing file, malformed YAML, no frontmatter); a
/// malformed block emits one stderr warning so a broken file is visible
/// instead of silently routing on stale data.
pub(crate) fn read_plan_frontmatter(plan_path: &Path) -> Map<String, Value> {
    let text = match std::fs::read_to_string(plan_path) {
        Ok(text) => text,
        Err(_) => return Map::new(),
    };
    let lines: Vec<&str> = text.lines().collect();
    if lines.first().map(|l| strip_bom_first_line(l)).as_deref() != Some("---") {
        return Map::new();
    }
    let end = match lines[1..].iter().position(|l| l.trim() == "---") {
        Some(i) => i + 1,
        None => return Map::new(),
    };
    let fm_text = lines[1..end].join("\n");
    match serde_yaml_ng::from_str::<Value>(&fm_text) {
        Ok(Value::Object(map)) => map,
        Ok(_) => Map::new(),
        Err(e) => {
            eprintln!(
                "warning: could not parse frontmatter in {}: {e}",
                plan_path.display()
            );
            Map::new()
        }
    }
}

/// True when the file OPENS a `---` block, whatever is in it: the one bit
/// the empty-map return of [`read_plan_frontmatter`] conflates (no block is
/// a valid state; a block that fails to parse is the undatable refusal).
fn has_frontmatter_block(plan_path: &Path) -> bool {
    let text = match std::fs::read_to_string(plan_path) {
        Ok(text) => text,
        Err(_) => return false,
    };
    match text.lines().next() {
        Some(first) => strip_bom_first_line(first) == "---",
        None => false,
    }
}

/// The node ids a plan's frontmatter declares ownership of: `claims:` (scalar
/// or list) and the `node:` key, which 330 of 807 plans carry instead. Empty
/// means no claim declared - only a positive claim can act.
fn plan_claims(plan_path: &Path) -> Vec<String> {
    let fm = read_plan_frontmatter(plan_path);
    let mut out: Vec<String> = Vec::new();
    if let Some(claims) = fm.get("claims") {
        let mut push = |v: &Value| {
            if let Some(s) = v.as_str() {
                let t = s.trim();
                if !t.is_empty() && t != "null" && !out.contains(&t.to_string()) {
                    out.push(t.to_string());
                }
            }
        };
        match claims {
            Value::String(_) => push(claims),
            Value::Array(list) => {
                for item in list {
                    push(item);
                }
            }
            _ => {}
        }
    }
    if let Some(node) = fm.get("node").and_then(Value::as_str) {
        let t = node.trim();
        if !t.is_empty() && t != "null" && !out.iter().any(|c| c == t) {
            out.push(t.to_string());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The depends module's minimal frontmatter reader (graph/depends.py)
// ---------------------------------------------------------------------------

/// Parse a minimal YAML frontmatter block: scalar string values and
/// list-of-strings values written with `- ` prefixes. Sufficient for the
/// title/blocked_by fields intake cares about; the full-YAML reader below
/// handles the claims vocabulary. Returns None on read failure (with a
/// stderr warning) and an empty map when no fenced block is present.
fn parse_depends_frontmatter(plan_path: &Path) -> Option<Map<String, Value>> {
    let text = match std::fs::read_to_string(plan_path) {
        Ok(text) => text,
        Err(e) => {
            eprintln!(
                "Warning: could not read frontmatter from {}: {e}",
                plan_path.display()
            );
            return None;
        }
    };
    Some(parse_depends_frontmatter_text(&text))
}

fn parse_depends_frontmatter_text(text: &str) -> Map<String, Value> {
    let mut result = Map::new();
    let body = match frontmatter_body(text) {
        Some(body) => body,
        None => return result,
    };
    let mut current_key: Option<String> = None;
    for raw_line in body.lines() {
        let stripped = raw_line.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        if raw_line.ltrim().starts_with("- ") {
            if let Some(key) = &current_key {
                let mut value = raw_line.ltrim()[2..].trim().to_string();
                if let Some(pos) = value.find(" #") {
                    value.truncate(pos);
                    value = value.trim_end().to_string();
                }
                value = value.trim_matches('"').trim_matches('\'').to_string();
                let entry = result
                    .entry(key.clone())
                    .or_insert_with(|| Value::Array(Vec::new()));
                if !entry.is_array() {
                    *entry = Value::Array(Vec::new());
                }
                if !value.is_empty() {
                    if let Value::Array(list) = entry {
                        list.push(Value::String(value));
                    }
                }
            }
            continue;
        }
        if raw_line.contains(':') && !raw_line.starts_with(' ') {
            let mut halves = raw_line.splitn(2, ':');
            let key = halves.next().unwrap_or("").trim().to_string();
            let mut val = halves.next().unwrap_or("").trim().to_string();
            if let Some(pos) = val.find(" #") {
                val.truncate(pos);
                val = val.trim_end().to_string();
            }
            let val = val.trim_matches('"').trim_matches('\'').to_string();
            current_key = Some(key.clone());
            result.insert(
                key,
                if val.is_empty() {
                    Value::Null
                } else {
                    Value::String(val)
                },
            );
        }
    }
    result
}

/// The text between the opening `---` line and the closing one, or None.
fn frontmatter_body(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

trait LtrimStr {
    fn ltrim(&self) -> &str;
}

impl LtrimStr for str {
    fn ltrim(&self) -> &str {
        self.trim_start()
    }
}

/// The first H1 heading in a markdown file, skipping frontmatter. Read
/// failures warn (an unreadable plan is visible in the intake output) and
/// fall through to the filename-derived title.
fn first_h1(plan_path: &Path) -> Option<String> {
    let text = match std::fs::read_to_string(plan_path) {
        Ok(text) => text,
        Err(e) => {
            eprintln!(
                "Warning: could not read {} for H1: {e}",
                plan_path.display()
            );
            return None;
        }
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0usize;
    if lines.first().is_some_and(|l| *l == "---") {
        i = 1;
        while i < lines.len() && lines[i] != "---" {
            i += 1;
        }
        i += 1;
    }
    while i < lines.len() {
        if let Some(rest) = lines[i].strip_prefix("# ") {
            return Some(rest.trim().to_string());
        }
        i += 1;
    }
    None
}

/// Parse a simple `[a, b, c]` inline YAML list: bare tokens, quoted strings,
/// and empty lists. Nested brackets or escaped commas return None so the
/// caller warns and skips instead of dropping edges silently.
fn parse_inline_yaml_list(src: &str) -> Option<Vec<String>> {
    let s = src.trim();
    if !(s.starts_with('[') && s.ends_with(']')) {
        return None;
    }
    let inner = s[1..s.len() - 1].trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    if inner.contains('[') || inner.contains(']') {
        return None;
    }
    let mut parts = Vec::new();
    for token in inner.split(',') {
        let mut t = token.trim().to_string();
        if t.is_empty() {
            continue;
        }
        if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
            || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
        {
            t = t[1..t.len() - 1].to_string();
        }
        if !t.is_empty() {
            parts.push(t);
        }
    }
    Some(parts)
}

fn declares_values(raw: Option<&Value>) -> bool {
    match raw {
        Some(Value::Array(list)) => list
            .iter()
            .any(|x| !x.as_str().unwrap_or("").trim().is_empty()),
        Some(Value::String(s)) => {
            let t = s.trim();
            !t.is_empty() && t != "[]"
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Title derivation and node lookup (_derive_title / _dashless_variant)
// ---------------------------------------------------------------------------

/// Python's str.title(): capitalize each alpha that follows a non-alpha.
fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut boundary = true;
    for ch in s.chars() {
        if ch.is_alphabetic() {
            if boundary {
                out.extend(ch.to_uppercase());
            } else {
                out.extend(ch.to_lowercase());
            }
            boundary = false;
        } else {
            boundary = true;
            out.push(ch);
        }
    }
    out
}

/// `^(\d{1,2}[a-zA-Z]?)-` as a filename prefix strip for the title ladder.
fn strip_title_prefix(stem: &str) -> &str {
    let bytes = stem.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && bytes[i].is_ascii_digit() && i < 2 {
        i += 1;
    }
    if i == 0 {
        return stem;
    }
    if i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'-' {
        &stem[i..]
    } else {
        stem
    }
}

/// Derive a display title: explicit override, frontmatter title (the depends
/// module's minimal reader, the same reader the ladder always used), first
/// H1, then the filename slug with the numeric prefix stripped.
fn derive_title(plan_path: &Path, override_title: Option<&str>) -> String {
    if let Some(ov) = override_title {
        if !ov.is_empty() {
            return ov.to_string();
        }
    }
    if let Some(fm) = parse_depends_frontmatter(plan_path) {
        if let Some(t) = fm.get("title").and_then(Value::as_str) {
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    if let Some(h1) = first_h1(plan_path) {
        return h1;
    }
    let stem = plan_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let slug = title_case(strip_title_prefix(&stem).replace('-', " ").trim());
    if slug.is_empty() {
        plan_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| plan_path.display().to_string())
    } else {
        slug
    }
}

/// The dash-less spelling of a node id (`x-bbbb` -> `xbbbb`): the minter
/// briefly minted dash-less ids, and the graph confirms the id, not the shape.
fn dashless_variant(node_id: &str) -> String {
    node_id.replacen('-', "", 1)
}

/// Exact-id row lookup with the dash-less twin as fallback. The wheel's
/// short-`ab-` fuzzy hop is a user-input concern; intake only ever looks up
/// full stored ids (minted ids, and a claimed node's `parent`).
fn find_node_exact<'a>(entries: &'a [Value], node_id: &str) -> Option<&'a Value> {
    entries
        .iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some(node_id))
        .or_else(|| {
            let twin = dashless_variant(node_id);
            entries
                .iter()
                .find(|e| e.get("id").and_then(Value::as_str) == Some(twin.as_str()))
        })
}

// ---------------------------------------------------------------------------
// The difficulty gate (plan/schema.py: _parse_created + difficulty_gate_error)
// ---------------------------------------------------------------------------

/// Date the difficulty band became required. A literal, never computed: a
/// runtime date crosses the gate on a commit nobody touched and leaves no
/// literal to grep (the smoke-rest outage of 2026-08-27).
const DIFFICULTY_REQUIRED_AFTER: &str = "2026-08-26";

/// `^\d{4}-\d{2}-\d{2}(?:[T ].*)?$` - extended-calendar forms only.
fn iso_created(s: &str) -> bool {
    if s.len() < 10 {
        return false;
    }
    let date_part = &s[..10];
    let rest = &s[10..];
    let parts: Vec<&str> = date_part.split('-').collect();
    if parts.len() != 3
        || parts[0].len() != 4
        || parts[1].len() != 2
        || parts[2].len() != 2
        || !parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    rest.is_empty() || rest.starts_with('T') || rest.starts_with(' ')
}

/// Days in month, leap-aware: `date.fromisoformat` refuses an impossible
/// day ("2026-02-31") and the port must refuse it too, or a plan Python
/// dates as undatable mints in Rust.
fn valid_ymd(year: i32, month: u32, day: u32) -> bool {
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let dim = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return false,
    };
    (1..=dim).contains(&day)
}

/// Read a frontmatter `created` value as (year, month, day), or None. Exact
/// extended-calendar strings only; an unquoted YAML int (20260827) and every
/// other scalar shape refuse rather than date the plan in one scope and
/// break it in another.
fn parse_created(value: Option<&Value>) -> Option<(i32, u32, u32)> {
    let s = value?.as_str()?.trim().to_string();
    if !iso_created(&s) {
        return None;
    }
    let mut parts = s[..10].split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

/// The date-keyed difficulty refusal for raw frontmatter, or None. The
/// minting lanes have no fallback dater, so undatable frontmatter refuses
/// (undatable_refuses is the default and the only scope this port serves).
fn difficulty_gate_error(fm: &Map<String, Value>) -> Option<String> {
    if fm.is_empty() {
        return None;
    }
    let created_raw = fm.get("created");
    let created = parse_created(created_raw);
    if created.is_none() {
        if created_raw.is_none() {
            return Some(
                "cannot read created: absent from frontmatter; the difficulty \
                 gate needs a date. Set created: <YYYY-MM-DD> and, for \
                 post-gate plans, difficulty: low, medium, high."
                    .to_string(),
            );
        }
        return Some(format!(
            "cannot read created: {}; the difficulty gate needs a date. Set \
             created: <YYYY-MM-DD> and, for post-gate plans, difficulty: \
             low, medium, high.",
            yaml_display(created_raw.unwrap_or(&Value::Null))
        ));
    }
    let (y, m, d) = created.unwrap();
    if !valid_ymd(y, m, d) {
        return Some(format!(
            "cannot read created: {}; the difficulty gate needs a date. Set \
             created: <YYYY-MM-DD> and, for post-gate plans, difficulty: \
             low, medium, high.",
            yaml_display(created_raw.unwrap_or(&Value::Null))
        ));
    }
    if (y, m, d) <= (2026, 8, 26) {
        return None;
    }
    let band = fm.get("difficulty").and_then(Value::as_str);
    match band {
        Some(b) if ["low", "medium", "high"].contains(&b.trim().to_lowercase().as_str()) => None,
        _ => Some(format!(
            "difficulty is required for plans created after \
             {DIFFICULTY_REQUIRED_AFTER}: set it to one of low, medium, high \
             (an intrinsic property of the work - the expected time, edge \
             cases, unknowns, and what a senior tech lead makes of the \
             problem; not a model or capacity hint)"
        )),
    }
}

/// The stderr rendering Python's `{value!r}` gives a frontmatter scalar.
fn yaml_display(v: &Value) -> String {
    match v {
        Value::String(s) => format!("'{s}'"),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Path normalization (os.path.normpath / abspath equivalents)
// ---------------------------------------------------------------------------

/// Lexical normpath: collapse `//`, `.`, `..`; keep the leading `/`.
fn normpath(p: &str) -> String {
    let absolute = p.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if !parts.is_empty() && parts[parts.len() - 1] != ".." {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// Lexical abspath against `cwd`.
pub(crate) fn abspath_cwd(p: &str, cwd: &Path) -> String {
    if p.starts_with('/') {
        normpath(p)
    } else {
        let joined = cwd.join(p);
        normpath(&joined.to_string_lossy())
    }
}

/// Python's `Path.resolve()`: symlink-true when the path exists, lexical
/// abspath when it does not (Python resolves the longest existing prefix;
/// plan-dep candidates rarely sit behind a symlinked prefix).
fn resolve_path(p: &str, cwd: &Path) -> String {
    let abs = abspath_cwd(p, cwd);
    match std::fs::canonicalize(&abs) {
        Ok(real) => real.to_string_lossy().into_owned(),
        Err(_) => abs,
    }
}

// ---------------------------------------------------------------------------
// Frontmatter depends collection (graph/depends.py)
// ---------------------------------------------------------------------------

/// Read a plan's declared blockers from its frontmatter. `blocked_by` is the
/// canonical key and wins when both it and the legacy `depends_on` carry
/// values; the ignored key is named on stderr. Returns (raw_values, plan_dir).
fn collect_frontmatter_depends(plan_path: &str) -> (Vec<String>, PathBuf) {
    let p = Path::new(plan_path);
    if !p.is_file() {
        return (
            Vec::new(),
            p.parent().map(Path::to_path_buf).unwrap_or_default(),
        );
    }
    let plan_dir = p.parent().map(Path::to_path_buf).unwrap_or_default();
    let Some(fm) = parse_depends_frontmatter(p) else {
        return (Vec::new(), plan_dir);
    };
    if fm.is_empty() {
        return (Vec::new(), plan_dir);
    }
    let canonical = fm.get("blocked_by");
    let legacy = fm.get("depends_on");
    let canon_set = declares_values(canonical);
    let legacy_set = declares_values(legacy);
    if canon_set && legacy_set {
        eprintln!(
            "Warning: {plan_path} declares both blocked_by and depends_on; \
             using blocked_by and ignoring depends_on. Migrate the plan \
             (`fno do plan migrate-keys`)."
        );
    }
    let raw = if canon_set { canonical } else { legacy };
    let Some(raw) = raw else {
        return (Vec::new(), plan_dir);
    };
    match raw {
        Value::Array(list) => (
            list.iter()
                .filter_map(|x| x.as_str())
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect(),
            plan_dir,
        ),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                return (Vec::new(), plan_dir);
            }
            if t.starts_with('[') && t.ends_with(']') {
                return match parse_inline_yaml_list(t) {
                    Some(parsed) => (parsed, plan_dir),
                    None => {
                        eprintln!(
                            "Warning: {plan_path} declares blockers in an \
                             inline-list form this parser can't read (nested \
                             brackets or escaped commas). Switch to block \
                             form (`- entry` lines)."
                        );
                        (Vec::new(), plan_dir)
                    }
                };
            }
            (vec![t.to_string()], plan_dir)
        }
        _ => (Vec::new(), plan_dir),
    }
}

/// Resolve depends_on references to node ids. Each raw value may be a
/// `<prefix>-hex` id, a plan path or slug, or a bare sequence token (`01`,
/// `02a`) resolved only against entries in the SAME directory as the plan -
/// batch-scoped so tokens never collide across projects. Numeric tokens are
/// normalized both ways (`3` matches `03-foo.md`). Returns (resolved,
/// unresolved); the caller warns on each unresolved raw value.
fn resolve_depends_on(
    raw_values: &[String],
    entries: &[Value],
    plan_dir: &Path,
) -> (Vec<String>, Vec<String>) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let id_set: std::collections::HashSet<&str> = entries
        .iter()
        .filter_map(|e| e.get("id").and_then(Value::as_str))
        .collect();
    let mut by_path: std::collections::HashMap<String, String> = Default::default();
    let mut by_slug: std::collections::HashMap<String, String> = Default::default();
    let mut by_sequence_in_dir: std::collections::HashMap<String, String> = Default::default();
    let plan_dir_abs = abspath_cwd(&plan_dir.to_string_lossy(), &cwd);
    for e in entries {
        let Some(eid) = e.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(pp) = e.get("plan_path").and_then(Value::as_str) else {
            continue;
        };
        let norm = normpath(pp);
        // Set, not keep-first: the wheel's `by_path[norm] = eid` lets a later
        // duplicate overwrite, so resolution lands on the newest row.
        by_path.insert(norm.clone(), eid.to_string());
        // Stored-relative plan paths join the entry's own cwd into the index,
        // so dependents holding a resolved abs path still match.
        let mut abs_form: Option<String> = None;
        if let Some(c) = e.get("cwd").and_then(Value::as_str) {
            if !norm.starts_with('/') {
                let joined = normpath(&Path::new(c).join(&norm).to_string_lossy());
                by_path
                    .entry(joined.clone())
                    .or_insert_with(|| eid.to_string());
                abs_form = Some(joined);
            }
        }
        let slug = norm
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string();
        if !slug.is_empty() {
            by_slug.entry(slug).or_insert_with(|| eid.to_string());
        }
        // Parent-folder indexing fires only on a hyphenated folder name: a
        // generic container like `plans/` would collide across every node.
        let parent = Path::new(&norm)
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if parent.contains('-') {
            by_slug.entry(parent).or_insert_with(|| eid.to_string());
        }
        // Batch-scoped sequence index, both stored spellings of the dir.
        let mut entry_dirs: Vec<String> = vec![match Path::new(&norm).parent() {
            Some(d) => normpath(&d.to_string_lossy()),
            None => ".".to_string(),
        }];
        if let Some(a) = &abs_form {
            if let Some(d) = Path::new(a).parent() {
                entry_dirs.push(normpath(&d.to_string_lossy()));
            }
        }
        if entry_dirs.contains(&plan_dir_abs) {
            let basename = norm.rsplit('/').next().unwrap_or("");
            if let Some(token) = sequence_token(basename) {
                by_sequence_in_dir
                    .entry(token.clone())
                    .or_insert_with(|| eid.to_string());
                if token.bytes().all(|b| b.is_ascii_digit()) {
                    let stripped = token.trim_start_matches('0');
                    let stripped = if stripped.is_empty() { "0" } else { stripped };
                    by_sequence_in_dir
                        .entry(stripped.to_string())
                        .or_insert_with(|| eid.to_string());
                    let zfilled = format!("{:0>2}", token);
                    by_sequence_in_dir
                        .entry(zfilled)
                        .or_insert_with(|| eid.to_string());
                }
            }
        }
    }
    let mut resolved: Vec<String> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();
    for raw in raw_values {
        if super::node_ref::has_node_id_prefix(raw) {
            if id_set.contains(raw.as_str()) {
                resolved.push(raw.clone());
            } else {
                unresolved.push(raw.clone());
            }
            continue;
        }
        let mut matched: Option<String> = None;
        let mut candidates = vec![raw.clone()];
        if !raw.starts_with('/') {
            let joined = plan_dir.join(raw);
            candidates.push(resolve_path(&joined.to_string_lossy(), &cwd));
        }
        for c in &candidates {
            let norm = normpath(c);
            if let Some(hit) = by_path.get(&norm) {
                matched = Some(hit.clone());
                break;
            }
        }
        if matched.is_none() {
            let slug = raw
                .trim_end_matches('/')
                .trim_end_matches(std::path::MAIN_SEPARATOR)
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string();
            if !slug.is_empty() {
                if let Some(hit) = by_slug.get(&slug) {
                    matched = Some(hit.clone());
                }
            }
        }
        if matched.is_none() {
            if let Some(hit) = by_sequence_in_dir.get(raw.as_str()) {
                matched = Some(hit.clone());
            }
        }
        match matched {
            Some(m) => resolved.push(m),
            None => unresolved.push(raw.clone()),
        }
    }
    (resolved, unresolved)
}

// ---------------------------------------------------------------------------
// Identity: filename/frontmatter/CLI claim agreement
// ---------------------------------------------------------------------------

/// The node id a plan basename encodes (`...-x7758-abc12345.md` -> `abc12345`),
/// scoped to `prefixes` when given. `_PLAN_NODE_RE` is a suffix match on the
/// filename, and a `#fragment` on the path never participates.
fn plan_filename_node_id(path: &str, prefixes: &[String]) -> Option<String> {
    use regex::Regex;
    use std::sync::OnceLock;
    static PLAN_NODE_RE: OnceLock<Regex> = OnceLock::new();
    let plan_node_re = PLAN_NODE_RE.get_or_init(|| {
        Regex::new(r"-(?P<prefix>[a-z][a-z0-9]{0,7})-(?P<hex>[0-9a-f]{4,8})\.md$").unwrap()
    });
    let base = path.split('#').next().unwrap_or(path);
    let name = Path::new(base).file_name()?.to_string_lossy();
    let caps = plan_node_re.captures(&name)?;
    let prefix = caps.name("prefix")?.as_str().to_string();
    if !prefixes.is_empty() && !prefixes.iter().any(|p| p.trim_end_matches('-') == prefix) {
        return None;
    }
    Some(format!(
        "{}-{}",
        caps.name("prefix")?.as_str(),
        caps.name("hex")?.as_str()
    ))
}

/// Refuse identity disagreement across filename, frontmatter, and CLI claim.
fn refuse_filename_claim_mismatch(plan_path: &str, cli_claim: Option<&str>) -> Result<(), String> {
    let declared: Vec<String> = plan_claims(Path::new(plan_path))
        .into_iter()
        .filter(|c| super::node_ref::is_wellformed_node_id(c))
        .collect();
    let mut prefixes: Vec<String> = vec!["ab".to_string(), super::settings::node_id_prefix()];
    for c in &declared {
        let head = c.split('-').next().unwrap_or("").to_string();
        if !head.is_empty() && !prefixes.contains(&head) {
            prefixes.push(head);
        }
    }
    if let Some(cc) = cli_claim {
        if super::node_ref::is_wellformed_node_id(cc) {
            let head = cc.split('-').next().unwrap_or("").to_string();
            if !head.is_empty() && !prefixes.contains(&head) {
                prefixes.push(head);
            }
        }
    }
    let Some(filename_node) = plan_filename_node_id(plan_path, &prefixes) else {
        return Ok(());
    };
    if declared.len() == 1 {
        let frontmatter_node = declared[0].clone();
        if filename_node != frontmatter_node {
            return Err(format!(
                "plan {plan_path} filename names {filename_node}, but frontmatter \
                 claims {frontmatter_node}; rename the file or correct the claim"
            ));
        }
    }
    if let Some(cc) = cli_claim {
        if super::node_ref::is_wellformed_node_id(cc) && filename_node != cc {
            return Err(format!(
                "plan {plan_path} filename names {filename_node}, but --claims names \
                 {cc}; rename the file or pass the matching claim"
            ));
        }
    }
    Ok(())
}

/// The intake verdict for one plan: already on the graph, claiming an
/// existing node, or ready to mint.
pub(crate) enum IntakeOutcome {
    Already {
        id: String,
        title: String,
    },
    Claim {
        id: String,
        title: String,
        claim_source: &'static str,
        spec: IntakeSpec,
    },
    Ready {
        spec: IntakeSpec,
    },
}

pub(crate) struct IntakeSpec {
    pub plan_path: String,
    pub roadmap_id: Option<String>,
    pub title: String,
    pub priority: String,
    pub blocks_everything: bool,
    pub deps: Vec<String>,
    pub points: Option<i64>,
    pub cli_project: Option<String>,
}

/// Resolve a plan's claim to an existing node. CLI flag wins and is strict;
/// frontmatter is lenient (well-formed ids only, silent skip on junk) and
/// refuses on two DISTINCT well-formed ids. Exact spelling first, then the
/// dash-less twin - the graph confirms the id, not the spelling.
fn resolve_claim(
    cli_claim: Option<&str>,
    plan_path: &str,
    entries: &[Value],
) -> Result<(Option<Value>, Option<&'static str>), String> {
    let mut fm_claim: Option<String> = None;
    if cli_claim.is_none() {
        let mut claimed: Vec<String> = plan_claims(Path::new(plan_path))
            .into_iter()
            .filter(|c| super::node_ref::is_wellformed_node_id(c))
            .collect();
        claimed.sort();
        claimed.dedup();
        if claimed.len() > 1 {
            return Err(format!(
                "plan {plan_path} names {} nodes across node:/claims: ({}); a plan \
                 file is one delivery unit, one plan is one PR is one node; pass \
                 --claims <id> to name the one node this plan claims",
                claimed.len(),
                claimed.join(", ")
            ));
        }
        fm_claim = claimed.into_iter().next();
    }
    let raw = cli_claim
        .or(fm_claim.as_deref())
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if raw.is_empty() {
        return Ok((None, None));
    }
    if !super::node_ref::is_wellformed_node_id(&raw) {
        return Err(format!(
            "invalid claims value: '{}' (expected a <prefix>-<4..8 hex> node id)",
            raw
        ));
    }
    let source: &'static str = if cli_claim.is_some() {
        "cli"
    } else {
        "frontmatter"
    };
    let mut wanted = vec![raw.clone()];
    let twin = dashless_variant(&raw);
    if twin != raw {
        wanted.push(twin);
    }
    for w in wanted {
        if let Some(e) = entries
            .iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(w.as_str()))
        {
            return Ok((Some(e.clone()), Some(source)));
        }
    }
    Err(format!(
        "claims target {raw} not found on graph - check the ID or remove the claim"
    ))
}

/// `--deps` ids must exist on the graph before anything else runs.
fn validate_cli_deps(cli_deps: &[String], entries: &[Value]) -> Result<(), String> {
    if cli_deps.is_empty() {
        return Ok(());
    }
    let id_set: std::collections::HashSet<&str> = entries
        .iter()
        .filter_map(|e| e.get("id").and_then(Value::as_str))
        .collect();
    let mut refused = false;
    for d in cli_deps {
        if !id_set.contains(d.as_str()) {
            eprintln!("Error: dependency not found in graph: {d}");
            refused = true;
        }
    }
    if refused {
        return Err(String::new());
    }
    Ok(())
}

/// The ledger sidecar row matching a plan path (entries[].plan_path,
/// normpath-compared). Read failures warn; a missing row only downgrades the
/// title/points fallbacks.
fn lookup_ledger_entry(plan_path: &str) -> Option<Value> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let ledger = crate::paths_cli::ledger_json(&cwd);
    if !ledger.exists() {
        return None;
    }
    let data: Value = match std::fs::read_to_string(&ledger)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
    {
        Some(d) => d,
        None => {
            eprintln!(
                "Warning: could not read {}: malformed ledger - intake will \
                 fall back to filename-derived title/points",
                ledger.display()
            );
            return None;
        }
    };
    let target = normpath(plan_path);
    data.get("entries")?
        .as_array()?
        .iter()
        .find(|e| {
            e.get("plan_path")
                .and_then(Value::as_str)
                .map(|ep| normpath(ep) == target)
                .unwrap_or(false)
        })
        .cloned()
}

/// The same-roadmap already-intaked match (plan_path equality).
fn match_plan_in_graph(
    entries: &[Value],
    plan_path: &str,
    roadmap_id: Option<&str>,
) -> Option<Value> {
    let target = normpath(plan_path);
    entries
        .iter()
        .find(|e| {
            let ep_ok = e
                .get("plan_path")
                .and_then(Value::as_str)
                .map(|ep| normpath(ep) == target)
                .unwrap_or(false);
            let rm_ok = e
                .get("roadmap_id")
                .and_then(Value::as_str)
                .map(|r| Some(r) == roadmap_id)
                .unwrap_or(roadmap_id.is_none());
            ep_ok && rm_ok
        })
        .cloned()
}

// ---------------------------------------------------------------------------
// Project/cwd resolution (resolve_git_roots / resolve_node_project_and_cwd)
// ---------------------------------------------------------------------------

/// Project inference from graph entries: an entry whose cwd IS the canonical
/// repo root names the project; else the first entry living under it. Tilde
/// cwd forms expand first or historical rows never match.
fn detect_project_entries(entries: &[Value], canonical_root: &str) -> Option<String> {
    let norm_root = normpath(canonical_root);
    let root_with_sep = format!("{}/", norm_root.trim_end_matches('/'));
    let mut fallback: Option<String> = None;
    for e in entries {
        let Some(entry_cwd) = e.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        if entry_cwd.trim().is_empty() {
            continue;
        }
        let expanded = if let Some(rest) = entry_cwd.strip_prefix("~/") {
            format!("{}/{}", std::env::var("HOME").unwrap_or_default(), rest)
        } else {
            entry_cwd.to_string()
        };
        let norm_cwd = normpath(&expanded);
        if norm_cwd == norm_root {
            return e.get("project").and_then(Value::as_str).map(String::from);
        }
        if fallback.is_none() && norm_cwd.starts_with(&root_with_sep) {
            fallback = e.get("project").and_then(Value::as_str).map(String::from);
        }
    }
    fallback
}

/// The work-map path declared for `project` (explicit sources only: this is
/// where an explicitly named project's cwd comes from).
pub(crate) fn project_root_from_settings(project: &str) -> Option<String> {
    for doc in super::settings::config_candidates() {
        for (name, raw) in super::create_cli::work_pairs(&doc) {
            if name == project {
                return Some(
                    super::create_cli::expand_home(&raw)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    None
}

/// Every project name declared across settings candidates - the unknown-
/// project warning's comparison set.
fn list_known_projects() -> std::collections::BTreeSet<String> {
    let mut known = std::collections::BTreeSet::new();
    for doc in super::settings::config_candidates() {
        for (name, _) in super::create_cli::work_pairs(&doc) {
            known.insert(name);
        }
    }
    known
}

fn warn_unknown_project(project: Option<&str>, known: Option<&std::collections::BTreeSet<String>>) {
    let Some(project) = project.filter(|p| !p.is_empty()) else {
        return;
    };
    let computed;
    let known = match known {
        Some(k) => k,
        None => {
            computed = list_known_projects();
            &computed
        }
    };
    if known.is_empty() || known.contains(project) {
        return;
    }
    eprintln!(
        "warning: project '{project}' is not declared in any settings.yaml \
         workspace; work-routing may not happen"
    );
}

/// Resolve (project, cwd, frontmatter) for a node from a plan path.
/// project = CLI > frontmatter > settings match on the canonical root >
/// basename of the main checkout (never the worktree name); cwd =
/// frontmatter (expanduser) > the work-map root of an explicitly named
/// project > the canonical main checkout. A node outlives the worktree it
/// was filed from, so the recorded cwd is the main checkout.
fn resolve_node_project_and_cwd(
    plan_path: &str,
    cli_project: Option<&str>,
    entries: &[Value],
) -> (Option<String>, String, Map<String, Value>) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let canonical_root = super::create_cli::repo_root(&cwd);
    let fm = read_plan_frontmatter(Path::new(plan_path));

    let fm_project_raw = fm.get("project");
    let fm_project = fm_project_raw
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    if fm_project_raw.is_some() && fm_project.is_none() {
        eprintln!(
            "warning: rejecting non-string project value in plan frontmatter: {}",
            yaml_display(fm_project_raw.unwrap_or(&Value::Null))
        );
    }
    let detected = detect_project_entries(entries, &canonical_root);
    let derived_name = Path::new(&canonical_root)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned());
    let project = cli_project
        .map(String::from)
        .or(fm_project.clone())
        .or(detected)
        .or(derived_name);

    let fm_cwd_raw = fm.get("cwd");
    let fm_cwd = fm_cwd_raw
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            if let Some(rest) = s.strip_prefix("~/") {
                format!("{}/{}", std::env::var("HOME").unwrap_or_default(), rest)
            } else {
                s.to_string()
            }
        });
    if fm_cwd_raw.is_some() && fm_cwd.is_none() {
        eprintln!(
            "warning: rejecting non-string cwd value in plan frontmatter: {}",
            yaml_display(fm_cwd_raw.unwrap_or(&Value::Null))
        );
    }
    // The work-map root only answers when the project came from an explicit
    // source (CLI flag or frontmatter); detected/derived projects already
    // carry a consistent cwd.
    let explicit = cli_project.is_some() || fm_project.is_some();
    let node_cwd = match fm_cwd {
        Some(c) => c,
        None => {
            if explicit {
                project
                    .as_deref()
                    .and_then(project_root_from_settings)
                    .unwrap_or_else(|| canonical_root.clone())
            } else {
                canonical_root.clone()
            }
        }
    };
    (project, node_cwd, fm)
}

// ---------------------------------------------------------------------------
// Node construction (_build_intake_node)
// ---------------------------------------------------------------------------

/// The intake-born node: the wheel's field set, with doc->graph flow for
/// type/size (the plan frontmatter is where they are declared), mission
/// context preserved verbatim, and the difficulty gate's band write via the
/// one canonical append shape every difficulty writer shares.
fn build_intake_node(spec: &IntakeSpec, entries: &[Value]) -> Result<Value, String> {
    let (project, node_cwd, fm) =
        resolve_node_project_and_cwd(&spec.plan_path, spec.cli_project.as_deref(), entries);
    let now = crate::graph_store::now_isoformat();

    let mission_id = fm
        .get("mission_id")
        .map(flexible_display)
        .filter(|s| !s.is_empty());
    let mission_wave = match fm.get("mission_wave") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)) {
            Some(n) => Some(n),
            None => match v.as_str().and_then(|s| s.trim().parse::<i64>().ok()) {
                Some(n) => Some(n),
                None => {
                    eprintln!(
                        "warning: non-integer mission_wave {} in {} frontmatter; \
                         storing as null",
                        yaml_display(v),
                        spec.plan_path
                    );
                    None
                }
            },
        },
    };
    let mission_slug = fm
        .get("mission_slug")
        .map(flexible_display)
        .filter(|s| !s.is_empty());
    let mission_from_msg_id = fm
        .get("mission_from_msg_id")
        .map(flexible_display)
        .filter(|s| !s.is_empty());

    let raw_difficulty = fm.get("difficulty");
    if raw_difficulty.is_none() && fm.get("model_tier").is_some() {
        // The compat read died with the field; a plan still spelling the band
        // as model_tier must hear it lost, not lose it silently.
        eprintln!(
            "warning: {}: frontmatter model_tier is retired and no longer read; \
             set difficulty: low|medium|high to carry the band",
            spec.plan_path
        );
    }
    // A non-string difficulty (an unquoted YAML int or list) refuses here -
    // it must never mint as a silently bandless node.
    let difficulty = match raw_difficulty {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            Some(super::create_cli::normalize_difficulty(s).map_err(|_| {
                format!(
                    "{}: invalid difficulty '{}' (expected one of: low, medium, high)",
                    spec.plan_path, s
                )
            })?)
        }
        Some(other) => {
            return Err(format!(
                "{}: invalid difficulty {} (expected one of: low, medium, high)",
                spec.plan_path,
                yaml_display(other)
            ));
        }
    };
    let blocks_everything = fm_blocks_everything(&fm);
    super::create_cli::validate_priority(&spec.priority, blocks_everything).map_err(|r| {
        format!(
            "{}: {}",
            spec.plan_path,
            r.message.trim_start_matches("Error: ")
        )
    })?;

    let existing: std::collections::BTreeSet<String> = entries
        .iter()
        .filter_map(|e| e.get("id").and_then(Value::as_str))
        .map(String::from)
        .collect();
    let minted = super::create_cli::mint_node_id(&existing)
        .map_err(|e| format!("{}: mint failed: {e}", spec.plan_path))?;

    let plan_sources: Vec<String> = fm
        .get("sources")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(flexible_display)
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let evidence = if plan_sources.is_empty() {
        format!("plan:{}", spec.plan_path)
    } else {
        plan_sources.join("; ")
    };
    let evidence = evidence.chars().take(300).collect::<String>();
    // Intake keeps the plan's references as evidence and never claims a
    // requester the plan did not declare, so the birth record carries no
    // source_kind - the declaration layer this stamp intentionally skips.
    let record = crate::node_origin::BirthRecord {
        request_origin: None,
        source_kind: None,
        birth_channel: Some("intake".to_string()),
        origin_evidence: Some(evidence),
    };
    let pending: Result<usize, String> = Ok(0);
    let resolution = crate::node_origin::resolve(&record, &pending);

    let size = normalize_size(fm.get("size").and_then(Value::as_str));
    let node_type = normalize_type(fm.get("type").and_then(Value::as_str));
    let mut node = Map::new();
    node.insert("id".into(), Value::String(minted.clone()));
    node.insert("parent".into(), Value::Null);
    node.insert("title".into(), Value::String(spec.title.clone()));
    node.insert("type".into(), Value::String(node_type));
    node.insert(
        "project".into(),
        project.map(Value::String).unwrap_or(Value::Null),
    );
    node.insert("cwd".into(), Value::String(node_cwd));
    node.insert("priority".into(), Value::String(spec.priority.clone()));
    node.insert("blocks_everything".into(), Value::Bool(blocks_everything));
    node.insert("difficulty_history".into(), Value::Array(Vec::new()));
    node.insert("domain".into(), Value::String("code".into()));
    node.insert(
        "blocked_by".into(),
        Value::Array(spec.deps.iter().map(|d| Value::String(d.clone())).collect()),
    );
    node.insert("session_id".into(), Value::Null);
    node.insert("locked_at".into(), Value::Null);
    node.insert("completed_at".into(), Value::Null);
    node.insert("has_brief".into(), Value::Bool(false));
    node.insert(
        "roadmap_id".into(),
        spec.roadmap_id
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    node.insert("vision_path".into(), Value::Null);
    node.insert("details".into(), Value::Null);
    node.insert(
        "size".into(),
        size.map(Value::String).unwrap_or(Value::Null),
    );
    node.insert("batch".into(), Value::Null);
    node.insert("cost_usd".into(), Value::Null);
    node.insert("cost_sessions".into(), Value::Array(Vec::new()));
    node.insert("plan_path".into(), Value::String(spec.plan_path.clone()));
    node.insert("pr_number".into(), Value::Null);
    node.insert("pr_url".into(), Value::Null);
    node.insert("merge_status".into(), Value::Null);
    node.insert("artifact_url".into(), Value::Null);
    node.insert("completion_note".into(), Value::Null);
    node.insert(
        "points".into(),
        spec.points
            .map(|p| Value::Number(p.into()))
            .unwrap_or(Value::Null),
    );
    node.insert("source".into(), Value::String("intake".into()));
    node.insert(
        "request_origin".into(),
        Value::String(resolution.origin.as_str().to_string()),
    );
    node.insert(
        "origin_evidence".into(),
        resolution
            .evidence
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    node.insert("created_at".into(), Value::String(now.clone()));
    node.insert(
        "mission_id".into(),
        mission_id.map(Value::String).unwrap_or(Value::Null),
    );
    node.insert(
        "mission_wave".into(),
        mission_wave
            .map(|n| Value::Number(n.into()))
            .unwrap_or(Value::Null),
    );
    node.insert(
        "mission_slug".into(),
        mission_slug.map(Value::String).unwrap_or(Value::Null),
    );
    node.insert(
        "mission_from_msg_id".into(),
        mission_from_msg_id
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    // A bandless plan leaves the key ABSENT - a present None would read as a
    // clearable mirror and delete a band the doc authored later.
    if let Some(band) = difficulty {
        node.insert("difficulty".into(), Value::String(band.clone()));
        let history = node
            .entry("difficulty_history")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(list) = history {
            list.push(json!({"value": band, "source": "blueprint", "ts": now}));
        }
    }
    Ok(Value::Object(node))
}

/// YAML scalars display via str() in the wheel; accept numbers/bools too.
fn flexible_display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn fm_blocks_everything(fm: &Map<String, Value>) -> bool {
    match fm.get("blocks_everything") {
        Some(Value::Bool(true)) => true,
        Some(Value::String(s)) => s.trim().to_lowercase() == "true",
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Intake prep (_prepare_intake)
// ---------------------------------------------------------------------------

/// The legacy severity vocabulary, mapped rather than stored degraded.
fn migrate_priority(priority: &str) -> &str {
    match priority {
        "high" => "p1",
        "medium" => "p2",
        "low" => "p3",
        other => other,
    }
}

/// Validate then resolve one plan into an outcome. Claim resolution runs
/// FIRST so a claim on an existing node beats the plan_path-equality match;
/// the difficulty gate sits after the already-intaked short circuit so an
/// idempotent re-run stays a no-op even when the file is undatable.
#[allow(clippy::too_many_arguments)]
fn prepare_intake(
    plan_path: &str,
    entries: &[Value],
    roadmap_id: Option<&str>,
    cli_title: Option<&str>,
    cli_priority: Option<&str>,
    cli_deps: &[String],
    cli_points: Option<i64>,
    cli_project: Option<&str>,
    cli_claim: Option<&str>,
) -> Result<IntakeOutcome, String> {
    refuse_filename_claim_mismatch(plan_path, cli_claim)?;
    let (claim_node, claim_source) = resolve_claim(cli_claim, plan_path, entries)?;

    let existing = match_plan_in_graph(entries, plan_path, roadmap_id);
    if existing.is_none() {
        if let Some(owner) = crate::graph_store::plan_path_owner_conflict(
            entries,
            claim_node
                .as_ref()
                .and_then(|n| n.get("id"))
                .and_then(Value::as_str),
            Some(plan_path),
        ) {
            return Err(format!(
                "plan {plan_path} is already the delivery unit of {owner} (a plan \
                 is one PR is one node, tracked under another roadmap); point the \
                 work at {owner}, or repoint deliberately with `fno backlog update \
                 <id> plan_path={plan_path} --force`"
            ));
        }
    }

    let (fm_raw, plan_dir) = collect_frontmatter_depends(plan_path);
    let fm = read_plan_frontmatter(Path::new(plan_path));
    let (resolved_fm, unresolved_fm) = resolve_depends_on(&fm_raw, entries, &plan_dir);
    for u in &unresolved_fm {
        eprintln!("Warning: depends_on '{u}' for {plan_path} not found on graph - skipping edge");
    }
    let mut all_deps: Vec<String> = Vec::new();
    for d in cli_deps.iter().chain(resolved_fm.iter()) {
        if !all_deps.contains(d) {
            all_deps.push(d.clone());
        }
    }

    let ledger_entry = lookup_ledger_entry(plan_path);
    let ledger_title = ledger_entry
        .as_ref()
        .and_then(|l| l.get("title"))
        .and_then(Value::as_str);
    let ledger_points = ledger_entry
        .as_ref()
        .and_then(|l| l.get("points"))
        .and_then(Value::as_i64);
    let title = if Path::new(plan_path).is_file() {
        derive_title(Path::new(plan_path), cli_title.or(ledger_title))
    } else {
        cli_title
            .or(ledger_title)
            .map(String::from)
            .unwrap_or_else(|| {
                Path::new(&normpath(plan_path))
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| plan_path.to_string())
            })
    };
    let points = cli_points.or(ledger_points);

    let priority = migrate_priority(
        cli_priority
            .or_else(|| fm.get("priority").and_then(Value::as_str))
            .unwrap_or("p2")
            .trim(),
    );
    if !["p0", "p1", "p2", "p3"].contains(&priority) {
        return Err(format!(
            "{plan_path}: invalid priority '{priority}'; must be one of p0, p1, p2, p3"
        ));
    }
    let blocks_everything = fm_blocks_everything(&fm);
    super::create_cli::validate_priority(priority, blocks_everything)
        .map_err(|r| format!("{plan_path}: {}", r.message.trim_start_matches("Error: ")))?;

    if let Some(existing) = existing {
        let existing_id = existing.get("id").and_then(Value::as_str).unwrap_or("");
        if let Some(claim) = &claim_node {
            let claim_id = claim.get("id").and_then(Value::as_str).unwrap_or("");
            if existing_id != claim_id {
                return Err(format!(
                    "plan_path already intaked as {existing_id}, but --claims names \
                     {claim_id}. Remove or supersede {existing_id} before claiming, \
                     e.g.:\n  fno backlog supersede {claim_id} --replaces {existing_id} \
                     --reason \"...\""
                ));
            }
        }
        return Ok(IntakeOutcome::Already {
            id: existing_id.to_string(),
            title,
        });
    }

    let gate_error = difficulty_gate_error(&fm);
    if gate_error.is_none() && fm.is_empty() && has_frontmatter_block(Path::new(plan_path)) {
        // "No block" is fine; a block present but unparseable is undatable,
        // and the empty-map carve-out must not mint a bandless node from
        // broken YAML.
        return Err(format!(
            "{plan_path}: cannot parse frontmatter; the difficulty gate cannot \
             date the plan. Fix the YAML block, then set created: <YYYY-MM-DD> \
             and, for post-gate plans, difficulty: low, medium, high."
        ));
    }
    if let Some(gate) = gate_error {
        return Err(format!("{plan_path}: {gate}"));
    }

    let spec = IntakeSpec {
        plan_path: plan_path.to_string(),
        roadmap_id: roadmap_id.map(String::from),
        title,
        priority: priority.to_string(),
        blocks_everything,
        deps: all_deps,
        points,
        cli_project: cli_project.map(String::from),
    };
    match claim_node {
        Some(claim) => {
            let id = claim
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let title = spec.title.clone();
            Ok(IntakeOutcome::Claim {
                id,
                title,
                claim_source: claim_source.unwrap_or("cli"),
                spec,
            })
        }
        None => Ok(IntakeOutcome::Ready { spec }),
    }
}

// ---------------------------------------------------------------------------
// The claim lane (_apply_claim_in_place)
// ---------------------------------------------------------------------------

/// The ONE canonical band write: drain the retired `model_tier` spelling on
/// the same write (a leftover manufactures a both-spellings row no verb can
/// clean), keep a bandless row's key absent, and attribute every write.
fn write_canonical_difficulty(
    row: &mut Map<String, Value>,
    band: Option<&str>,
    source: &str,
    ts: &str,
    history_on: &str,
) {
    let prior = row
        .get("difficulty")
        .and_then(Value::as_str)
        .map(String::from);
    row.remove("model_tier");
    if band.is_some() || row.contains_key("difficulty") {
        row.insert(
            "difficulty".into(),
            band.map(String::from)
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
    }
    if history_on == "always" || prior.as_deref() != band {
        let history = row
            .entry("difficulty_history")
            .or_insert_with(|| Value::Array(Vec::new()));
        if !history.is_array() {
            *history = Value::Array(Vec::new());
        }
        if let Value::Array(list) = history {
            list.push(json!({"value": band, "source": source, "ts": ts}));
        }
    }
}

/// Bind a claimed node to the plan in place, in any state: attach the plan,
/// merge the doc-declared fields, promote idea -> ready. Shared shape with
/// the multi lane so a claim lands identically from either surface.
fn apply_claim_in_place(
    es: &mut [Value],
    claim_id: &str,
    plan_path: &str,
    spec: &IntakeSpec,
    project: Option<&str>,
) {
    let Some(idx) = es
        .iter()
        .position(|e| e.get("id").and_then(Value::as_str) == Some(claim_id))
    else {
        return;
    };
    let frontmatter = read_plan_frontmatter(Path::new(plan_path));
    let claimed_type = normalize_type(frontmatter.get("type").and_then(Value::as_str));
    let raw_difficulty = frontmatter.get("difficulty").and_then(Value::as_str);
    if raw_difficulty.is_none() && frontmatter.get("model_tier").is_some() {
        eprintln!(
            "warning: {plan_path}: frontmatter model_tier is retired and no \
             longer read; set difficulty: low|medium|high to carry the band"
        );
    }
    // Read the row's parent (and the epic-cap verdict) before the mutable
    // borrow: the wheel looks the parent up live, but a claim never edits
    // its own parent edge, so the pre-read is the same answer.
    let parent_id = es[idx]
        .get("parent")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let parent_node = if parent_id.is_empty() {
        None
    } else {
        find_node_exact(es, &parent_id).cloned()
    };
    let epic_exceeds = match &parent_node {
        Some(parent) => {
            let mut probe = es[idx].clone();
            if let Some(o) = probe.as_object_mut() {
                o.insert("type".into(), Value::String("epic".into()));
            }
            super::node_ref::would_exceed_epic_depth(es, &probe, parent)
        }
        None => false,
    };
    // The null-project/cwd backfill resolves from the same immutable read -
    // computed before the mutable borrow, applied inside it. Only fills
    // nulls; never overwrites existing values.
    let needs_backfill = es[idx].get("project").map(Value::is_null).unwrap_or(true)
        || es[idx].get("cwd").map(Value::is_null).unwrap_or(true);
    let backfill = if needs_backfill {
        let (resolved_project, resolved_cwd, _) =
            resolve_node_project_and_cwd(plan_path, project, es);
        Some((resolved_project, resolved_cwd))
    } else {
        None
    };

    let Some(obj) = es[idx].as_object_mut() else {
        return;
    };
    obj.insert("plan_path".into(), Value::String(plan_path.to_string()));
    obj.insert("title".into(), Value::String(spec.title.clone()));
    if let Some(raw) = raw_difficulty {
        match super::create_cli::normalize_difficulty(raw) {
            Ok(revised) => {
                let now = crate::graph_store::now_isoformat();
                write_canonical_difficulty(obj, Some(&revised), "blueprint", &now, "always");
            }
            Err(_) => {
                eprintln!(
                    "warning: invalid difficulty '{raw}' (expected one of: low, \
                     medium, high); difficulty left unchanged"
                );
            }
        }
    }
    if claimed_type != DEFAULT_NODE_TYPE
        && obj.get("type").and_then(Value::as_str) != Some(claimed_type.as_str())
    {
        // `add`/`update` refuse a third epic level; the doc-frontmatter lane
        // SKIPS instead - the doc is advisory and the claim itself is valid.
        if claimed_type == "epic" && parent_node.is_some() && epic_exceeds {
            let current = obj.get("type").and_then(Value::as_str).unwrap_or("?");
            eprintln!(
                "warning: plan declares type: epic but {claim_id} sits under \
                 {parent_id}; promoting it would exceed the epic-nesting cap - \
                 type left as '{current}'"
            );
        } else {
            obj.insert("type".into(), Value::String(claimed_type.clone()));
        }
    }
    if !spec.deps.is_empty() {
        let mut merged: Vec<String> = obj
            .get("blocked_by")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        for d in &spec.deps {
            if !merged.contains(d) {
                merged.push(d.clone());
            }
        }
        obj.insert(
            "blocked_by".into(),
            Value::Array(merged.into_iter().map(Value::String).collect()),
        );
    }
    // Only override priority if the plan supplied a non-default one.
    if spec.priority != "p2" {
        obj.insert("priority".into(), Value::String(spec.priority.clone()));
    }
    if spec.priority == "p0" && spec.blocks_everything {
        obj.insert("blocks_everything".into(), Value::Bool(true));
    }
    if let Some(points) = spec.points {
        obj.insert("points".into(), Value::Number(points.into()));
    }
    // Backfill null project/cwd only; never overwrite existing values.
    if let Some((resolved_project, resolved_cwd)) = backfill {
        if obj.get("project").map(Value::is_null).unwrap_or(true) {
            if let Some(p) = resolved_project {
                obj.insert("project".into(), Value::String(p));
            }
        }
        if obj.get("cwd").map(Value::is_null).unwrap_or(true) {
            obj.insert("cwd".into(), Value::String(resolved_cwd));
        }
    }
}

// ---------------------------------------------------------------------------
// File-surface detection (graph/collision.py, the parse half)
// ---------------------------------------------------------------------------

/// Headings under which a plan's file list lives; "file ownership map" is
/// the section /blueprint writes.
const FILE_HEADINGS: [&str; 5] = [
    "files to modify",
    "files to change",
    "files touched",
    "file ownership map",
    "files",
];

/// Normalize one file cell: strip parenthetical annotations, backticks, and
/// a trailing `:42` / `:42-44` line suffix.
fn strip_path_cell(raw: &str) -> String {
    use regex::Regex;
    use std::sync::OnceLock;
    static PAREN: OnceLock<Regex> = OnceLock::new();
    static LINE_SUFFIX: OnceLock<Regex> = OnceLock::new();
    let paren = PAREN.get_or_init(|| Regex::new(r"\s*\([^)]*\)\s*$").unwrap());
    let line_suffix = LINE_SUFFIX.get_or_init(|| Regex::new(r":\d+(-\d+)?$").unwrap());
    let s = raw.trim();
    let s = paren.replace(s, "").trim().to_string();
    let s = s.replace('`', "").trim().to_string();
    line_suffix.replace(&s, "").trim().to_string()
}

/// A table row that is not the header separator.
fn is_file_table_row(line: &str) -> bool {
    use regex::Regex;
    use std::sync::OnceLock;
    static SEPARATOR: OnceLock<Regex> = OnceLock::new();
    let separator = SEPARATOR.get_or_init(|| Regex::new(r"^\|[\s:|-]+\|[\s:|-]*$").unwrap());
    if !line.starts_with('|') {
        return false;
    }
    !separator.is_match(line)
}

/// Walk lines after a recognized heading until the next `##` heading or EOF,
/// collecting column-1 paths from any markdown tables.
fn extract_files_from_section(
    text: &str,
    heading_index: usize,
) -> std::collections::BTreeSet<String> {
    use regex::Regex;
    use std::sync::OnceLock;
    static SEPARATOR: OnceLock<Regex> = OnceLock::new();
    let separator = SEPARATOR.get_or_init(|| Regex::new(r"^\|[\s:|-]+\|").unwrap());
    let mut files = std::collections::BTreeSet::new();
    let mut saw_header = false;
    let mut saw_separator = false;
    for line in text.lines().skip(heading_index + 1) {
        let stripped = line.trim();
        if stripped.starts_with("##") {
            break;
        }
        if !is_file_table_row(stripped) {
            if saw_header && saw_separator && stripped.is_empty() {
                saw_header = false;
                saw_separator = false;
            }
            continue;
        }
        if !saw_header {
            saw_header = true;
            continue;
        }
        if !saw_separator {
            saw_separator = true;
            if separator.is_match(stripped) {
                continue;
            }
        }
        let body = stripped.strip_prefix('|').unwrap_or(stripped);
        let parts: Vec<&str> = body.split('|').collect();
        if parts.is_empty() {
            continue;
        }
        let cell = strip_path_cell(parts[0]);
        if !cell.is_empty() {
            files.insert(cell);
        }
    }
    files
}

/// Scan one markdown file for its files-to-modify set.
fn scan_one_file_surface(path: &Path) -> std::collections::BTreeSet<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "Warning: collision parser cannot read {}: {e}",
                path.display()
            );
            return Default::default();
        }
    };
    let mut found = std::collections::BTreeSet::new();
    for (idx, line) in text.lines().enumerate() {
        let stripped = line.trim();
        if !stripped.starts_with("##") {
            continue;
        }
        let heading_text = stripped.trim_start_matches('#').trim().to_lowercase();
        if FILE_HEADINGS.iter().any(|h| heading_text.starts_with(h)) {
            found.extend(extract_files_from_section(&text, idx));
        }
    }
    found
}

/// True when a plan states a file surface the collision check can compare;
/// false means UNEVALUATED, not clean.
fn has_file_surface(plan_path: &Path) -> bool {
    if plan_path.is_dir() {
        // A folder plan spreads its tables across 00-INDEX.md and phase files.
        let mut children: Vec<PathBuf> = std::fs::read_dir(plan_path)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension()
                            .map(|x| x.to_str() == Some("md"))
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default();
        children.sort();
        return children
            .iter()
            .any(|c| !scan_one_file_surface(c).is_empty());
    }
    if !plan_path.exists() {
        return false;
    }
    !scan_one_file_surface(plan_path).is_empty()
}

// ---------------------------------------------------------------------------
// CLI layer (`fno backlog intake`)
// ---------------------------------------------------------------------------

struct IntakeArgs {
    plan_paths: Vec<String>,
    from_list: Option<String>,
    roadmap_id: Option<String>,
    title: Option<String>,
    priority: Option<String>,
    deps: Option<String>,
    points: Option<i64>,
    project: Option<String>,
    force_new_roadmap: bool,
    batch: bool,
    dry_run: bool,
    claims: Option<String>,
    allow_no_surface: bool,
}

/// The `fno backlog intake` dispatcher arm: pull an existing plan file in as
/// a backlog node. One delivery unit per plan: plan == PR == node.
pub fn run_intake(tail: &[String]) -> i32 {
    if tail.is_empty() {
        print!("{}", include_str!("intake_help.txt"));
        return 0;
    }
    if tail.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", include_str!("intake_help.txt"));
        return 0;
    }
    let mut args = IntakeArgs {
        plan_paths: Vec::new(),
        from_list: None,
        roadmap_id: None,
        title: None,
        priority: None,
        deps: None,
        points: None,
        project: None,
        force_new_roadmap: false,
        batch: false,
        dry_run: false,
        claims: None,
        allow_no_surface: false,
    };
    let mut i = 0usize;
    while i < tail.len() {
        let arg = tail[i].as_str();
        let takes_value: Option<&str> = match arg {
            "--from" => Some("--from"),
            "--roadmap-id" => Some("--roadmap-id"),
            "--title" | "-t" => Some("--title"),
            "--priority" | "-p" => Some("--priority"),
            "--deps" => Some("--deps"),
            "--points" => Some("--points"),
            "--project" => Some("--project"),
            "--claims" => Some("--claims"),
            _ => None,
        };
        if let Some(name) = takes_value {
            match tail.get(i + 1) {
                Some(v) => {
                    let v = v.clone();
                    match name {
                        "--from" => args.from_list = Some(v),
                        "--roadmap-id" => args.roadmap_id = Some(v),
                        "--title" => args.title = Some(v),
                        "--priority" => args.priority = Some(v),
                        "--deps" => args.deps = Some(v),
                        "--points" => match v.trim().parse::<i64>() {
                            Ok(n) => args.points = Some(n),
                            Err(_) => {
                                eprintln!("Error: --points must be an integer, got '{v}'");
                                return 2;
                            }
                        },
                        "--project" => args.project = Some(v),
                        "--claims" => args.claims = Some(v),
                        _ => unreachable!(),
                    }
                    i += 2;
                }
                None => {
                    eprintln!("Error: {name} requires a value");
                    return 2;
                }
            }
            continue;
        }
        match arg {
            "--force-new-roadmap" => {
                args.force_new_roadmap = true;
                i += 1;
            }
            "--batch" => {
                args.batch = true;
                i += 1;
            }
            "--dry-run" | "-N" => {
                args.dry_run = true;
                i += 1;
            }
            "--allow-no-surface" => {
                args.allow_no_surface = true;
                i += 1;
            }
            _ => {
                if arg.starts_with('-') && arg.len() > 1 {
                    eprintln!("Error: No such option: {arg}");
                    return 2;
                }
                args.plan_paths.push(arg.to_string());
                i += 1;
            }
        }
    }
    intake_impl(&args)
}

fn intake_impl(args: &IntakeArgs) -> i32 {
    // Reject the removed --batch flag.
    if args.batch {
        eprintln!(
            "Error: `--batch` was removed. Use multi-path intake instead:\n  \
             fno backlog intake plans/a.md plans/b.md plans/c.md\n  \
             fno backlog intake plans/folder/*.md  # shell glob"
        );
        return 1;
    }
    if let Some(p) = &args.project {
        if p.trim().is_empty() {
            eprintln!("Error: --project must be a non-empty string");
            return 1;
        }
    }
    if let Some(p) = &args.priority {
        if !["p0", "p1", "p2", "p3"].contains(&p.as_str()) {
            eprintln!("Error: invalid priority '{p}'. Must be: p0, p1, p2, p3");
            return 1;
        }
    }
    let all_paths = collect_intake_paths(&args.plan_paths, args.from_list.as_deref());
    if all_paths.is_empty() {
        if let Some(from) = &args.from_list {
            let label = if from == "-" { "stdin" } else { from.as_str() };
            eprintln!(
                "Error: --from {label} produced 0 usable paths (blank lines and \
                 '#' comments are skipped)."
            );
        } else {
            eprintln!(
                "Error: no plan paths provided. Pass one or more positional \
                 arguments, or use --from FILE (or --from -)."
            );
        }
        return 1;
    }
    if all_paths.len() > 1 {
        return do_intake_multi(args, &all_paths);
    }
    intake_single(args, &all_paths[0])
}

/// Positional paths plus `--from FILE|-`; a comma-joined arg that names no
/// file splits on commas (the wheel's forgiving-paste fallback).
fn collect_intake_paths(plan_paths: &[String], from_list: Option<&str>) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    if let Some(from) = from_list {
        let raw = if from == "-" {
            let mut buf = String::new();
            if std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).is_err() {
                eprintln!("Error: --from -: could not read stdin");
                std::process::exit(1);
            }
            buf
        } else {
            match std::fs::read_to_string(from) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("Error: --from {from}: {e}");
                    std::process::exit(1);
                }
            }
        };
        for line in raw.lines() {
            let s = line.trim();
            if s.is_empty() || s.starts_with('#') {
                continue;
            }
            paths.push(s.to_string());
        }
    }
    for p in plan_paths {
        if p.contains(',') && !Path::new(p).exists() {
            for part in p.split(',') {
                let part = part.trim();
                if !part.is_empty() {
                    paths.push(part.to_string());
                }
            }
        } else {
            paths.push(p.clone());
        }
    }
    paths
}

/// The one birth-lane refusal every creation surface carries: on an external
/// tracker backend the item lives there, and a graph mint would be a phantom.
fn refuse_create_on_external_backend() -> bool {
    let backend = std::env::var("FNO_TRACKER_BACKEND")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "graph".to_string());
    if backend == "graph" {
        return false;
    }
    eprintln!(
        "fno backlog: creating work belongs to the {backend} tracker. Create the \
         item there; footnote tracks it by its id (e.g. /fno:target owner/repo#N)."
    );
    true
}

/// `~` expansion for CLI-given plan paths: intake reads the CLI spelling, but
/// a quoted tilde path must still resolve (the collision resolver expands it
/// at dispatch, so refusing it here would break a working plan).
pub(crate) fn expand_user(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).join(rest).to_string_lossy().into_owned();
        }
    }
    p.to_string()
}

/// The single-plan lane: prepare, refuse surfaceless, preview or write.
fn intake_single(args: &IntakeArgs, plan_path: &str) -> i32 {
    let cli_deps: Vec<String> = args
        .deps
        .as_deref()
        .map(|d| {
            d.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    if refuse_create_on_external_backend() {
        return 1;
    }
    let graph = super::settings::graph_path();
    let entries = match crate::graph_store::read_rows(&graph) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("Error: the graph store is unreadable: {e}");
            return 1;
        }
    };
    if let Some(roadmap_id) = &args.roadmap_id {
        if !args.force_new_roadmap
            && !entries
                .iter()
                .any(|e| e.get("roadmap_id").and_then(Value::as_str) == Some(roadmap_id.as_str()))
        {
            eprintln!(
                "unknown roadmap_id: {roadmap_id} (use /megawalk vision.md to create \
                 a roadmap first, pass --force-new-roadmap, or omit --roadmap-id to \
                 intake to the backlog)"
            );
            return 2;
        }
    }
    if let Err(m) = validate_cli_deps(&cli_deps, &entries) {
        if !m.is_empty() {
            eprintln!("Error: {m}");
        }
        return 1;
    }
    let prep = match prepare_intake(
        plan_path,
        &entries,
        args.roadmap_id.as_deref(),
        args.title.as_deref(),
        args.priority.as_deref(),
        &cli_deps,
        args.points,
        args.project.as_deref(),
        args.claims.as_deref(),
    ) {
        Ok(prep) => prep,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    if let IntakeOutcome::Already { id, .. } = &prep {
        println!("already intaked: {id}");
        return 0;
    }
    // Surface is mandatory at intake; the refusal also covers the dry-run
    // preview, so it runs after prep validates and before any write. A
    // missing plan refuses in this lane (the multi lane warns-and-skips).
    let expanded = expand_user(plan_path);
    if !Path::new(&expanded).exists() {
        eprintln!(
            "Error: plan file not found: {plan_path}. Check the path spelling; a \
             missing plan has no file table either."
        );
        return 2;
    }
    if !args.allow_no_surface && !has_file_surface(Path::new(&expanded)) {
        eprintln!(
            "Error: {plan_path} states no comparable file surface (## Files to \
             Modify parses empty or the file does not decode as UTF-8); add rows \
             or pass --allow-no-surface"
        );
        return 2;
    }

    if args.dry_run {
        let (verb, title, deps, claim_id) = match &prep {
            IntakeOutcome::Claim {
                id, title, spec, ..
            } => ("claim", title.clone(), spec.deps.clone(), Some(id.clone())),
            IntakeOutcome::Ready { spec } => {
                ("intake", spec.title.clone(), spec.deps.clone(), None)
            }
            IntakeOutcome::Already { .. } => unreachable!("handled above"),
        };
        println!("{verb} preview (dry-run, no changes):");
        let target = claim_id
            .map(|id| format!(" (claims {id})"))
            .unwrap_or_default();
        println!("  would {verb}: \"{title}\"  (plan: {plan_path}){target}");
        if !deps.is_empty() {
            println!("  blocked_by: {}", deps.join(", "));
        }
        return 0;
    }
    intake_commit(args, &graph, plan_path, prep)
}

/// The write half of the single lane: one locked mutate (claim or mint), the
/// receipt, then the advisory post steps in the wheel's order.
fn intake_commit(args: &IntakeArgs, graph: &Path, plan_path: &str, prep: IntakeOutcome) -> i32 {
    let base_version = crate::graph_store::base_version(graph).unwrap_or_default();
    if let IntakeOutcome::Claim {
        id,
        title,
        claim_source,
        spec,
    } = &prep
    {
        // Emit "not in ledger" before mutating.
        if lookup_ledger_entry(plan_path).is_none() {
            eprintln!("plan_path not in ledger.json - intake will continue anyway");
        }
        let mut landed = None;
        for retry in 0..6 {
            let mut working = match crate::graph_store::read_rows(graph) {
                Ok(rows) => rows,
                Err(e) => {
                    eprintln!("Error: the graph store is unreadable: {e}");
                    return 1;
                }
            };
            apply_claim_in_place(&mut working, id, plan_path, spec, args.project.as_deref());
            match crate::graph_store::locked_mutate(
                graph,
                crate::graph_store::MutateInput {
                    entries: working,
                    canonical_path: None,
                    base_version: base_version.clone(),
                    plan_rungs: None,
                },
                crate::graph_store::DEFAULT_LOCK_TIMEOUT,
            ) {
                Ok(_) => {
                    landed = Some(id.clone());
                    break;
                }
                Err(crate::graph_store::StoreError::Conflict) if retry < 5 => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(e) => {
                    eprintln!("Error: graph write failed: {e}");
                    return 1;
                }
            }
        }
        let Some(claim_id) = landed else {
            return 1;
        };
        println!(
            "linked plan to {claim_id} via {claim_source}: \"{title}\" - take the \
             work lock: fno do target start {claim_id}"
        );
        // Mirror nav fields onto the just-linked plan of the CLAIMED node;
        // additive, never wedges the claim.
        if let Ok(rows_now) = crate::graph_store::read_rows(graph) {
            crate::plan_doc::project::project_graph_nodes(
                &rows_now,
                std::slice::from_ref(&claim_id),
                None,
                None,
                None,
                None,
            );
        }
        return 0;
    }
    let IntakeOutcome::Ready { spec } = &prep else {
        unreachable!("claim handled above");
    };
    if lookup_ledger_entry(plan_path).is_none() {
        eprintln!("plan_path not in ledger.json - intake will continue anyway");
    }
    let mut minted: Option<String> = None;
    let mut built_node: Option<Value> = None;
    for retry in 0..6 {
        let rows = match crate::graph_store::read_rows(graph) {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("Error: the graph store is unreadable: {e}");
                return 1;
            }
        };
        let node = match build_intake_node(spec, &rows) {
            Ok(n) => n,
            Err(e) => {
                // Build-time refusals (the difficulty gate, an invalid band)
                // land as a clean one-line error on the single-file lane.
                eprintln!("error: intake refused: {e}");
                return 1;
            }
        };
        let id = node
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut working = rows.clone();
        working.push(node.clone());
        crate::graph_store::recompute_statuses_with_plan_rungs(&mut working, None);
        match crate::graph_store::locked_mutate(
            graph,
            crate::graph_store::MutateInput {
                entries: working,
                canonical_path: None,
                base_version: base_version.clone(),
                plan_rungs: None,
            },
            crate::graph_store::DEFAULT_LOCK_TIMEOUT,
        ) {
            Ok(_) => {
                minted = Some(id);
                built_node = Some(node);
                break;
            }
            Err(crate::graph_store::StoreError::Conflict) if retry < 5 => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("Error: graph write failed: {e}");
                return 1;
            }
        }
    }
    let (Some(new_id), Some(node)) = (minted, built_node) else {
        return 1;
    };
    let destination = args
        .roadmap_id
        .clone()
        .unwrap_or_else(|| "backlog".to_string());
    let title = node
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(spec.title.as_str());
    println!("intake {new_id} -> {destination}: \"{title}\"");
    intake_post_steps(graph, &new_id, &node);
    0
}

/// The advisory post steps: unknown-project warning, the filing-time dedup
/// net (live graph plus archived residents), the plan-doc projection, and
/// the born-with-why hook. None of them can fail the intake.
fn intake_post_steps(graph: &Path, new_id: &str, node: &Value) {
    let landed_project = node.get("project").and_then(Value::as_str);
    warn_unknown_project(landed_project, None);
    if let Ok(post_rows) = crate::graph_store::read_rows(graph) {
        let archive = super::settings::graph_path()
            .parent()
            .map(|p| p.join("graph-archive.json"))
            .unwrap_or_default();
        // The dedup lines name the same derived status word the live
        // readers present, and score against the archive too: a shipped
        // done node is the answer to a duplicate.
        let derived_rows: Vec<Value> =
            crate::graph_store::entries_with_archive(&post_rows, &archive)
                .iter()
                .map(|r| {
                    let mut r = r.clone();
                    if let Some(o) = r.as_object_mut() {
                        let derived = super::create_cli::derive_status(o, graph);
                        o.insert("status".into(), Value::String(derived));
                    }
                    r
                })
                .collect();
        if let Some(born) = derived_rows
            .iter()
            .find(|r| r.get("id").and_then(Value::as_str) == Some(new_id))
        {
            warn_similar_nodes_intake(born, &derived_rows, new_id);
        }
    }
    if let Ok(rows_now) = crate::graph_store::read_rows(graph) {
        let minted = vec![new_id.to_string()];
        crate::plan_doc::project::project_graph_nodes(&rows_now, &minted, None, None, None, None);
    }
    let _ = super::birth::on_node_born(graph, node, None);
}

/// The dedup receipt with the intake remedy: re-file with --claims names the
/// top candidate, any state.
fn warn_similar_nodes_intake(node: &Value, entries: &[Value], new_id: &str) {
    let candidates = super::relatedness::similar_nodes(node, entries, 3, None);
    if candidates.is_empty() {
        return;
    }
    let mut lines = vec![format!(
        "dedup: {count} similar existing node(s) for {new_id}:",
        count = candidates.len()
    )];
    for (cid, score, _reason) in &candidates {
        let cand = entries
            .iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(cid.as_str()));
        let status = cand
            .and_then(|c| c.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let mut title = cand
            .and_then(|c| c.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if title.chars().count() > 60 {
            title = title.chars().take(60).collect::<String>() + "...";
        }
        let pr = cand
            .and_then(|c| c.get("pr_number"))
            .and_then(Value::as_i64);
        let pr_tok = pr.map(|n| format!("  PR#{n}")).unwrap_or_default();
        lines.push(format!(
            "  {cid}  {status:<10}{score:.2}{pr_tok}  \"{title}\""
        ));
    }
    let top = candidates[0].0.clone();
    lines.push(format!(
        "consolidate: `fno backlog supersede` / `fno backlog update`, or re-file \
         with --claims {top}"
    ));
    eprintln!("{}", lines.join("\n"));
}

// ---------------------------------------------------------------------------
// The multi lane (`intake plans/*.md`)
// ---------------------------------------------------------------------------

fn do_intake_multi(args: &IntakeArgs, all_paths: &[String]) -> i32 {
    let cli_deps: Vec<String> = args
        .deps
        .as_deref()
        .map(|d| {
            d.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    if refuse_create_on_external_backend() {
        return 1;
    }
    let graph = super::settings::graph_path();
    let preview_entries = match crate::graph_store::read_rows(&graph) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("Error: the graph store is unreadable: {e}");
            return 1;
        }
    };
    if let Err(m) = validate_cli_deps(&cli_deps, &preview_entries) {
        if !m.is_empty() {
            eprintln!("Error: {m}");
        }
        return 1;
    }
    let mut resolved: Vec<(String, bool)> = Vec::new(); // (path, missing)
    for raw in all_paths {
        let missing = !Path::new(raw).exists();
        resolved.push((raw.clone(), missing));
    }
    let concrete: Vec<String> = resolved
        .iter()
        .filter(|(_, missing)| !missing)
        .map(|(p, _)| p.clone())
        .collect();
    if concrete.is_empty() {
        for (p, missing) in &resolved {
            if *missing {
                eprintln!("warning: not found, skipped: {p}");
            }
        }
        eprintln!(
            "Error: nothing to intake (0 of {len} paths resolved)",
            len = all_paths.len()
        );
        return 4;
    }
    // plan_path_owner_conflict compares normpath-only, so a CLI-relative
    // spelling never matches a graph-stored absolute plan_path; probe the
    // absolute spelling too, or a false refusal blames a file this run was
    // a no-op for.
    let cwd = std::env::current_dir().unwrap_or_default();
    let unbound = |f: &str| -> bool {
        let mut spellings = vec![f.to_string()];
        let expanded = expand_user(f);
        if !Path::new(&expanded).is_absolute() {
            spellings.push(resolve_path(&expanded, &cwd));
        }
        spellings.iter().all(|s| {
            crate::graph_store::plan_path_owner_conflict(&preview_entries, None, Some(s)).is_none()
        })
    };
    let to_check: Vec<String> = concrete.iter().filter(|f| unbound(f)).cloned().collect();
    for f in &to_check {
        if !args.allow_no_surface && !has_file_surface(Path::new(&expand_user(f))) {
            if !Path::new(&expand_user(f)).exists() {
                eprintln!(
                    "Error: plan file not found: {f}. Check the path spelling; a \
                     missing plan has no file table either."
                );
                return 2;
            }
            eprintln!(
                "Error: {f} states no comparable file surface (## Files to Modify \
                 parses empty or the file does not decode as UTF-8); add rows or \
                 pass --allow-no-surface"
            );
            return 2;
        }
    }
    if let Some(roadmap_id) = &args.roadmap_id {
        if !args.force_new_roadmap
            && !preview_entries
                .iter()
                .any(|e| e.get("roadmap_id").and_then(Value::as_str) == Some(roadmap_id.as_str()))
        {
            eprintln!(
                "unknown roadmap_id: {roadmap_id} (use /megawalk vision.md to create \
                 a roadmap first, pass --force-new-roadmap, or omit --roadmap-id to \
                 intake to the backlog)"
            );
            return 2;
        }
    }
    if args.dry_run {
        return multi_dry_run(args, all_paths, resolved, &preview_entries, &cli_deps);
    }
    multi_commit(args, resolved, &graph, &cli_deps)
}

/// Multi dry-run: validate-then-preview per file, mirroring the real run -
/// a refused plan previews as would-skip, a claimed plan mutates the preview
/// graph so a later duplicate previews its true outcome.
fn multi_dry_run(
    args: &IntakeArgs,
    all_paths: &[String],
    resolved: Vec<(String, bool)>,
    preview_entries: &[Value],
    cli_deps: &[String],
) -> i32 {
    println!(
        "Multi-intake preview (dry-run, no changes): {len} paths:",
        len = all_paths.len()
    );
    let mut preview = preview_entries.to_vec();
    let mut would = 0usize;
    for (raw, missing) in &resolved {
        if *missing {
            println!("  warning: not found, skipped: {raw}");
            continue;
        }
        let f = raw.as_str();
        match prepare_intake(
            f,
            &preview,
            args.roadmap_id.as_deref(),
            args.title.as_deref(),
            args.priority.as_deref(),
            cli_deps,
            args.points,
            args.project.as_deref(),
            args.claims.as_deref(),
        ) {
            Err(exc) => {
                eprintln!("  error: would skip {f}: {exc}");
            }
            Ok(IntakeOutcome::Already { id, title }) => {
                println!("  already intaked {id}: \"{title}\"  ({f})");
            }
            Ok(IntakeOutcome::Claim {
                id, title, spec, ..
            }) => {
                println!("  would claim: \"{title}\"  (plan: {f})  (claims {id})");
                apply_claim_in_place(&mut preview, &id, f, &spec, args.project.as_deref());
            }
            Ok(IntakeOutcome::Ready { spec }) => match build_intake_node(&spec, &preview) {
                Err(exc) => {
                    eprintln!("  error: would skip {f}: {exc}");
                }
                Ok(node) => {
                    preview.push(node);
                    let title = spec.title.clone();
                    println!("  would intake: \"{title}\"  (plan: {f})");
                    would += 1;
                }
            },
        }
    }
    println!("{would} plans would be intaked. Run without --dry-run to apply.");
    0
}

/// The real multi lane: one locked mutate over the batch; a refusal skips
/// one file with a clean per-file error so the rest of the batch lands.
fn multi_commit(
    args: &IntakeArgs,
    resolved: Vec<(String, bool)>,
    graph: &Path,
    cli_deps: &[String],
) -> i32 {
    let base_version = crate::graph_store::base_version(graph).unwrap_or_default();
    let concrete_count = resolved.iter().filter(|(_, m)| !m).count();
    println!("Multi-intake {concrete_count} plans:");
    let mut intaked = 0usize;
    let mut claimed = 0usize;
    let mut already = 0usize;
    let mut invalid = 0usize;
    let mut landed_projects: std::collections::BTreeSet<String> = Default::default();
    let mut new_ids: Vec<String> = Vec::new();
    let mut claimed_ids: Vec<String> = Vec::new();
    let mut working;
    for retry in 0..6 {
        working = match crate::graph_store::read_rows(graph) {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("Error: the graph store is unreadable: {e}");
                return 1;
            }
        };
        for (raw, missing) in &resolved {
            if *missing {
                eprintln!("  warning: not found, skipped: {raw}");
                continue;
            }
            let f = raw.as_str();
            let prep = match prepare_intake(
                f,
                &working,
                args.roadmap_id.as_deref(),
                args.title.as_deref(),
                args.priority.as_deref(),
                cli_deps,
                args.points,
                args.project.as_deref(),
                args.claims.as_deref(),
            ) {
                Err(exc) => {
                    invalid += 1;
                    eprintln!("  error: skipped {f}: {exc}");
                    continue;
                }
                Ok(prep) => prep,
            };
            match prep {
                IntakeOutcome::Already { id, title } => {
                    already += 1;
                    println!("  already intaked {id}: \"{title}\"  ({f})");
                }
                IntakeOutcome::Claim {
                    id,
                    title,
                    claim_source,
                    spec,
                } => {
                    // The claim lands on the existing idea node in place -
                    // never a fresh node beside an unclaimed idea.
                    apply_claim_in_place(&mut working, &id, f, &spec, args.project.as_deref());
                    claimed += 1;
                    claimed_ids.push(id.clone());
                    println!("  claim {id} via {claim_source}: \"{title}\"  ({f})");
                }
                IntakeOutcome::Ready { spec } => match build_intake_node(&spec, &working) {
                    Err(exc) => {
                        invalid += 1;
                        eprintln!("  error: skipped {f}: {exc}");
                    }
                    Ok(node) => {
                        if let Some(p) = node.get("project").and_then(Value::as_str) {
                            landed_projects.insert(p.to_string());
                        }
                        let id = node
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let title = spec.title.clone();
                        working.push(node);
                        intaked += 1;
                        new_ids.push(id.clone());
                        println!("  intake {id}: \"{title}\"  ({f})");
                    }
                },
            }
        }
        crate::graph_store::recompute_statuses_with_plan_rungs(&mut working, None);
        match crate::graph_store::locked_mutate(
            graph,
            crate::graph_store::MutateInput {
                entries: working,
                canonical_path: None,
                base_version: base_version.clone(),
                plan_rungs: None,
            },
            crate::graph_store::DEFAULT_LOCK_TIMEOUT,
        ) {
            Ok(_) => break,
            Err(crate::graph_store::StoreError::Conflict) if retry < 5 => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("Error: graph write failed: {e}");
                return 1;
            }
        }
    }
    multi_post_steps(graph, &claimed_ids, &new_ids, &landed_projects);
    let missing = resolved.iter().filter(|(_, m)| *m).count();
    println!(
        "\n{intaked} newly intaked, {claimed} claimed, {already} already intaked, \
         {missing} skipped.{refused}",
        refused = if invalid > 0 {
            format!(" {invalid} refused.")
        } else {
            String::new()
        }
    );
    if intaked + claimed + already == 0 {
        return 4;
    }
    0
}

/// Multi-lane advisory steps, in the wheel's order: the claimed nodes' nav
/// projection, the per-new-node dedup net, and the landed-projects warning.
fn multi_post_steps(
    graph: &Path,
    claimed_ids: &[String],
    new_ids: &[String],
    landed_projects: &std::collections::BTreeSet<String>,
) {
    if !claimed_ids.is_empty() {
        if let Ok(rows_now) = crate::graph_store::read_rows(graph) {
            crate::plan_doc::project::project_graph_nodes(
                &rows_now,
                claimed_ids,
                None,
                None,
                None,
                None,
            );
        }
    }
    if !new_ids.is_empty() {
        let archive = super::settings::graph_path()
            .parent()
            .map(|p| p.join("graph-archive.json"))
            .unwrap_or_default();
        if let Ok(post_rows) = crate::graph_store::read_rows(graph) {
            let derived_rows: Vec<Value> =
                crate::graph_store::entries_with_archive(&post_rows, &archive)
                    .iter()
                    .map(|r| {
                        let mut r = r.clone();
                        if let Some(o) = r.as_object_mut() {
                            let derived = super::create_cli::derive_status(o, graph);
                            o.insert("status".into(), Value::String(derived));
                        }
                        r
                    })
                    .collect();
            for nid in new_ids {
                if let Some(born) = derived_rows
                    .iter()
                    .find(|r| r.get("id").and_then(Value::as_str) == Some(nid.as_str()))
                {
                    warn_similar_nodes_intake(born, &derived_rows, nid);
                }
            }
        }
    }
    let known = list_known_projects();
    for proj in landed_projects {
        warn_unknown_project(Some(proj), Some(&known));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_tokens_cover_nn_nna_and_bare_forms() {
        assert_eq!(sequence_token("01-foo.md"), Some("01".to_string()));
        assert_eq!(sequence_token("02a-bar.md"), Some("02a".to_string()));
        assert_eq!(sequence_token("3-plan.md"), Some("3".to_string()));
        assert_eq!(sequence_token("intro.md"), None);
        assert_eq!(sequence_token("123-toolong.md"), None);
        assert_eq!(sequence_token("01_foo.md"), None);
    }

    #[test]
    fn iso_created_reads_extended_calendar_only() {
        assert!(iso_created("2026-09-17"));
        assert!(iso_created("2026-09-17T10:00:00"));
        assert!(iso_created("2026-09-17 10:00"));
        assert!(!iso_created("20260917"));
        assert!(!iso_created("2026-9-17"));
        assert!(!iso_created("2026-W38-3"));
        assert!(!iso_created("not-a-date"));
    }

    #[test]
    fn difficulty_gate_shape_matches_the_python_gate() {
        // Pre-gate: no band required.
        let pre =
            serde_yaml_ng::from_str::<Value>("created: 2026-08-01\ndifficulty: nope\n").unwrap();
        assert!(pre
            .as_object()
            .map(|m| difficulty_gate_error(m))
            .unwrap_or(None)
            .is_none());
        // Post-gate: band required.
        let post = serde_yaml_ng::from_str::<Value>("created: 2026-09-17\n").unwrap();
        let err = post
            .as_object()
            .map(|m| difficulty_gate_error(m))
            .unwrap_or(None);
        assert!(err.unwrap().contains("difficulty is required"));
        // Post-gate with a band passes.
        let banded =
            serde_yaml_ng::from_str::<Value>("created: 2026-09-17\ndifficulty: high\n").unwrap();
        assert!(banded
            .as_object()
            .map(|m| difficulty_gate_error(m))
            .unwrap_or(None)
            .is_none());
        // An impossible day refuses rather than dating.
        let bad_day = serde_yaml_ng::from_str::<Value>("created: 2026-02-31\n").unwrap();
        let err = bad_day
            .as_object()
            .map(|m| difficulty_gate_error(m))
            .unwrap_or(None);
        assert!(err.unwrap().contains("cannot read created"));
    }

    #[test]
    fn normpath_collapses_dot_segments_and_preserves_root() {
        assert_eq!(normpath("/a/b/../c//d"), "/a/c/d");
        assert_eq!(normpath("a/./b"), "a/b");
        assert_eq!(normpath("../x"), "../x");
        assert_eq!(normpath("/.."), "/");
        assert_eq!(normpath(""), ".");
    }

    #[test]
    fn filename_node_id_matches_the_suffix_shape() {
        let prefixes = vec!["ab".to_string(), "x".to_string()];
        assert_eq!(
            plan_filename_node_id("/tmp/20260917-removal-x-abc12345.md", &prefixes),
            Some("x-abc12345".to_string())
        );
        assert_eq!(plan_filename_node_id("/tmp/plan.md", &prefixes), None);
        assert_eq!(
            plan_filename_node_id("/tmp/plan-x-ABC12345.md", &prefixes),
            None
        );
        let scoped = vec!["ab".to_string()];
        assert_eq!(
            plan_filename_node_id("/tmp/plan-x-abc12345.md", &scoped),
            None
        );
    }

    #[test]
    fn title_ladder_prefers_frontmatter_then_h1_then_slug() {
        let dir = std::env::temp_dir().join("intake_rs_tests");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("01-remove-the-wheel.md");
        std::fs::write(
            &p,
            "---\ntitle: Doc Title\n---\n\n# Heading Title\n\nbody\n",
        )
        .unwrap();
        assert_eq!(derive_title(&p, None), "Doc Title");
        std::fs::write(&p, "---\n---\n\n# Heading Title\n\nbody\n").unwrap();
        assert_eq!(derive_title(&p, None), "Heading Title");
        std::fs::write(&p, "body only\n").unwrap();
        assert_eq!(derive_title(&p, None), "Remove The Wheel");
        assert_eq!(derive_title(&p, Some("Override")), "Override");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn inline_yaml_list_reads_bare_quoted_and_empty_forms() {
        assert_eq!(
            parse_inline_yaml_list("[ab-123, ab-456]"),
            Some(vec!["ab-123".to_string(), "ab-456".to_string()])
        );
        assert_eq!(parse_inline_yaml_list("[]"), Some(Vec::new()));
        assert_eq!(
            parse_inline_yaml_list("[\"path/a\", 'path/b']"),
            Some(vec!["path/a".to_string(), "path/b".to_string()])
        );
        assert_eq!(parse_inline_yaml_list("[a[b]"), None);
    }

    #[test]
    fn depends_frontmatter_reader_walks_block_lists() {
        let fm = parse_depends_frontmatter_text(
            "---\ntitle: T\nblocked_by:\n  - ab-11111111\n  - ab-22222222 # trailing\nsize: L\n---\n",
        );
        assert_eq!(
            fm.get("blocked_by"),
            Some(&Value::Array(vec![
                Value::String("ab-11111111".into()),
                Value::String("ab-22222222".into()),
            ]))
        );
        assert_eq!(fm.get("size"), Some(&Value::String("L".into())));
    }

    #[test]
    fn collect_depends_prefers_blocked_by_and_names_the_legacy_key() {
        let dir = std::env::temp_dir().join("intake_rs_tests2");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("both-keys.md");
        std::fs::write(
            &p,
            "---\nblocked_by:\n  - ab-33333333\ndepends_on:\n  - ab-44444444\n---\n",
        )
        .unwrap();
        let (values, plan_dir) = collect_frontmatter_depends(&p.display().to_string());
        assert_eq!(values, vec!["ab-33333333".to_string()]);
        assert_eq!(plan_dir, PathBuf::from(&dir));
        let _ = std::fs::remove_file(&p);
    }
}
