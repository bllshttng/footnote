//! `fno backlog decompose` - bounded epic decomposition into group children.
//!
//! Ported from cli/src/fno/graph/cli.py (cmd_decompose) and graph/_decompose.py.
//! Each group becomes one child node (parent=epic) bundling 1+ execution waves
//! into a single shippable PR; re-running with the same slugs upserts in place
//! (identity: parent == epic AND group_slug == slug, with legacy plan_path
//! fallbacks). The whole decomposition lands in one locked graph mutation, so
//! a bad spec leaves the graph exactly as it was.

use super::workflows::text_at;
use crate::plans_path::PinnedTimestamp;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The validated group spec. All fields are guaranteed present; `project` and
/// `cwd` are None when the group inherits the epic's repo.
#[derive(Debug)]
pub(crate) struct NormalizedGroup {
    pub slug: String,
    pub title: String,
    pub waves: String,
    pub blocked_by_groups: Vec<String>,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub dep: String,
    pub stub_against: Option<String>,
    pub needs_think: bool,
    pub adopt: Vec<String>,
}

/// Any invalid decomposition request; `exit_code` maps the class to the
/// documented codes (1 user error, 2 bad state/cycle, 3 not found).
#[derive(Debug)]
pub(crate) struct DecomposeErr {
    pub message: String,
    pub exit_code: i32,
}

impl DecomposeErr {
    fn new(message: impl Into<String>, exit_code: i32) -> Self {
        DecomposeErr {
            message: message.into(),
            exit_code,
        }
    }
}

impl From<String> for DecomposeErr {
    fn from(message: String) -> Self {
        DecomposeErr {
            message,
            exit_code: 1,
        }
    }
}

/// `^[A-Za-z0-9][A-Za-z0-9-]*$` - filesystem/URL-safe so the plan fragment
/// and the `.group-<slug>.md` filename stay stable.
fn group_slug_ok(slug: &str) -> bool {
    let bytes = slug.as_bytes();
    match bytes.first() {
        Some(b) if b.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
}

/// Interface-contract versions pinned in a design doc (`## Interface Contract`
/// carrying `**contract_version: N**` or `### Contract vN` subheadings), scoped
/// to the section body. An empty set means no pin: every `contract` dep falls
/// back to `hard`.
fn extract_contract_versions(doc_text: &str) -> std::collections::BTreeSet<u32> {
    use regex::Regex;
    use std::sync::OnceLock;
    static HEADING: OnceLock<Regex> = OnceLock::new();
    static VERSION: OnceLock<Regex> = OnceLock::new();
    static SUBHEADING: OnceLock<Regex> = OnceLock::new();
    let heading =
        HEADING.get_or_init(|| Regex::new(r"(?m)^##[ \t]+Interface Contract[ \t]*$").unwrap());
    let version =
        VERSION.get_or_init(|| Regex::new(r"\*\*contract_version:\s*(\d+)\s*\*\*").unwrap());
    let subheading =
        SUBHEADING.get_or_init(|| Regex::new(r"(?mi)^###[ \t]+Contract v(\d+)\b").unwrap());
    let mut versions = std::collections::BTreeSet::new();
    let Some(m) = heading.find(doc_text) else {
        return versions;
    };
    let mut body = &doc_text[m.end()..];
    let section_break = Regex::new(r"(?m)^##?[ \t]+").unwrap();
    if let Some(nxt) = section_break.find(body) {
        body = &body[..nxt.start()];
    }
    for cap in version.captures_iter(body) {
        if let Ok(n) = cap[1].parse::<u32>() {
            versions.insert(n);
        }
    }
    for cap in subheading.captures_iter(body) {
        if let Ok(n) = cap[1].parse::<u32>() {
            versions.insert(n);
        }
    }
    versions
}

/// Resolve a group's dependency tier to its persisted form:
/// `(dep, stub_against, contract_version, downgrade_reason)`. A `contract`
/// request with no pinned contract downgrades to `hard` and names why.
fn classify_group_dep(
    grp: &NormalizedGroup,
    pinned_versions: &std::collections::BTreeSet<u32>,
    base: &str,
) -> (String, Option<String>, Option<u32>, Option<String>) {
    if grp.dep != "contract" {
        return ("hard".to_string(), None, None, None);
    }
    if pinned_versions.is_empty() {
        return (
            "hard".to_string(),
            None,
            None,
            Some(format!(
                "group '{}' requested dep=contract but the epic doc pins no \
                 ## Interface Contract; falling back to hard serialization",
                grp.slug
            )),
        );
    }
    let version = *pinned_versions.iter().next_back().unwrap();
    let stub_against = grp
        .stub_against
        .clone()
        .unwrap_or_else(|| format!("{base}#interface-contract"));
    (
        "contract".to_string(),
        Some(stub_against),
        Some(version),
        None,
    )
}

// ---------------------------------------------------------------------------
// Plan addressing: the epic base doc and the child plan paths
// ---------------------------------------------------------------------------

/// Strip any `#fragment` from an epic plan_path to get the base doc path.
/// An epic with no plan_path cannot address group fragments; the remedy
/// names the folding verb.
fn plan_base(plan_path: Option<&str>) -> Result<String, DecomposeErr> {
    match plan_path {
        Some(pp) if !pp.is_empty() => Ok(pp.split('#').next().unwrap_or(pp).to_string()),
        _ => Err(DecomposeErr::new(
            "epic has no plan_path; cannot address group fragments. To fold \
             existing nodes into a plan-less epic run: fno backlog contain <epic> <id>...",
            1,
        )),
    }
}

/// Self-contained per-child plan path: `<dir>/<stem>.group-<slug>.md`. Pure
/// string derivation so the same slug maps to the same path on every run.
fn separate_plan_path(base: &str, slug: &str) -> String {
    let p = Path::new(base);
    let stem = if p
        .file_name()
        .map(|n| n.to_string_lossy().ends_with(".md"))
        .unwrap_or(false)
    {
        p.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    match p.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir
            .join(format!("{stem}.group-{slug}.md"))
            .to_string_lossy()
            .into_owned(),
        _ => format!("{stem}.group-{slug}.md"),
    }
}

/// The group slug of a child node, or None when it is not a group child of
/// `base`. Identity is the durable `group_slug` field; legacy children fall
/// back to their plan_path shape (the `#group-<slug>` fragment or the
/// `<stem>.group-<slug>.md` filename), matched on FILENAMES so an abs/rel
/// spelling mismatch between base and plan_path never hides a child.
fn group_child_slug(node: &Value, base: &str) -> Option<String> {
    if let Some(g) = node.get("group_slug").and_then(Value::as_str) {
        if !g.is_empty() {
            return Some(g.to_string());
        }
    }
    let pp = node.get("plan_path").and_then(Value::as_str).unwrap_or("");
    let p = Path::new(base);
    let base_name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = if base_name.ends_with(".md") {
        base_name.trim_end_matches(".md")
    } else {
        base_name.as_str()
    };
    if let Some(pos) = pp.find("#group-") {
        let base_part = &pp[..pos];
        let slug = &pp[pos + 7..];
        if Path::new(base_part)
            .file_name()
            .map(|n| n.to_string_lossy() == base_name)
            .unwrap_or(false)
        {
            return Some(slug.to_string());
        }
    }
    let name = Path::new(pp)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let sep_pre = format!("{stem}.group-");
    if let Some(rest) = name.strip_prefix(&sep_pre) {
        if let Some(slug) = rest.strip_suffix(".md") {
            return Some(slug.to_string());
        }
    }
    None
}

/// True when a node is a group child of SOME epic, independent of any base.
/// Adoption uses this unscoped predicate: a legacy child of a DIFFERENT epic
/// resolves None against this base, and adopting it would re-parent it away
/// from its own epic.
fn is_group_child(node: &Value) -> bool {
    if let Some(g) = node.get("group_slug").and_then(Value::as_str) {
        if !g.is_empty() {
            return true;
        }
    }
    let pp = node.get("plan_path").and_then(Value::as_str).unwrap_or("");
    if pp.contains("#group-") {
        return true;
    }
    let name = Path::new(pp)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some(pos) = name.find(".group-") {
        return name[pos..].ends_with(".md");
    }
    false
}

/// True when a group child node already has a PR / merge / completion signal:
/// re-decomposing past such a node would orphan shipped work.
fn is_shipped(node: &Value) -> bool {
    node.get("pr_number").map(|v| !v.is_null()).unwrap_or(false)
        || node
            .get("merge_status")
            .map(|v| !v.is_null())
            .unwrap_or(false)
        || node
            .get("completed_at")
            .map(|v| !v.is_null())
            .unwrap_or(false)
        || node
            .get("additional_prs")
            .map(|v| !v.is_null())
            .unwrap_or(false)
}

/// Existing group children of the epic whose slug is absent from the new
/// spec, in graph order.
fn find_orphans(
    entries: &[Value],
    epic_id: &str,
    base: &str,
    keep_slugs: &std::collections::BTreeSet<String>,
) -> Vec<Value> {
    entries
        .iter()
        .filter(|e| e.get("parent").and_then(Value::as_str) == Some(epic_id))
        .filter(|e| {
            group_child_slug(e, base)
                .map(|slug| !keep_slugs.contains(&slug))
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// The ceiling and the spec validation (resolve_effective_cap / validate_groups)
// ---------------------------------------------------------------------------

/// The absent-cap sentinel: `.get("max_children", UNSET)` distinguishes no
/// key from an explicit `max_children: null` (present-but-invalid, fail-closed).
pub(crate) enum MaxChildren {
    Unset,
    Value(i64),
    Invalid(String),
}

/// Resolve the decompose ceiling and a human label naming its source.
/// `max_children` (the epic author's durable cap) overrides the config
/// default upward; an explicit `--max-prs` may only tighten it. Fail-closed
/// on a present-but-invalid cap: a typo'd cap must fail loud, never protect
/// nothing.
fn resolve_effective_cap(
    max_children: &MaxChildren,
    explicit_max_prs: Option<i64>,
    config_default: Option<i64>,
    epic_doc_rel: Option<&str>,
) -> Result<(i64, String), DecomposeErr> {
    match max_children {
        MaxChildren::Unset => {
            if let Some(flag) = explicit_max_prs {
                return Ok((flag, format!("--max-prs {flag}")));
            }
            match config_default {
                Some(d) => Ok((d, format!("config default {d}"))),
                None => Err(DecomposeErr::new(
                    "could not read config.blueprint.max_prs_per_epic",
                    1,
                )),
            }
        }
        MaxChildren::Invalid(shown) => {
            let loc = epic_doc_rel
                .map(|r| format!(" (in {r})"))
                .unwrap_or_default();
            Err(DecomposeErr::new(
                format!(
                    "max_children in the epic frontmatter is invalid: {shown}{loc}; \
                     it must be an integer >= 1 (remove the key to fall back to \
                     --max-prs / config)"
                ),
                1,
            ))
        }
        MaxChildren::Value(n) if *n < 1 => {
            let loc = epic_doc_rel
                .map(|r| format!(" (in {r})"))
                .unwrap_or_default();
            Err(DecomposeErr::new(
                format!(
                    "max_children in the epic frontmatter is invalid: {n}{loc}; \
                     it must be an integer >= 1 (remove the key to fall back to \
                     --max-prs / config)"
                ),
                1,
            ))
        }
        MaxChildren::Value(n) => {
            let src = match epic_doc_rel {
                Some(rel) => format!("max_children={n} (from {rel})"),
                None => format!("max_children={n}"),
            };
            if let Some(flag) = explicit_max_prs {
                if flag < *n {
                    return Ok((flag, format!("--max-prs {flag} (tightening {src})")));
                }
            }
            Ok((*n, src))
        }
    }
}

/// The wave numbers a group's `waves` range string names: `4`, `1-2`, `1,3`,
/// `1-2,4`. Empty means "no slice", never wave zero.
fn wave_numbers(spec: &str) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    for part in spec.replace(' ', "").split(',') {
        if part.is_empty() {
            continue;
        }
        if part.contains('-') {
            let mut halves = part.splitn(2, '-');
            let lo = halves.next().unwrap_or("");
            let hi = halves.next().unwrap_or("");
            if let (Ok(lo), Ok(hi)) = (lo.parse::<u32>(), hi.parse::<u32>()) {
                if lo <= hi {
                    out.extend(lo..=hi);
                }
            }
            continue;
        }
        if let Ok(n) = part.parse::<u32>() {
            out.push(n);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The epic doc's `## Execution Strategy` section body, or None when the
/// section is missing. (The YAML parse of the body is the caller's job: the
/// wheel hands it to parse_execution_strategy; the port reads the waves and
/// tasks it needs directly.)
fn execution_strategy_body(text: &str) -> Option<String> {
    use regex::Regex;
    use std::sync::OnceLock;
    static SECTION: OnceLock<Regex> = OnceLock::new();
    // The regex crate has no look-around, so the section body is cut in two
    // steps: anchor the heading, then bound it at the next heading.
    let section =
        SECTION.get_or_init(|| Regex::new(r"(?m)^##[ \t]+Execution Strategy[ \t]*$").unwrap());
    let m = section.find(text)?;
    let body = &text[m.end()..];
    let breaker = Regex::new(r"(?m)^##?[ \t]+").unwrap();
    let cut = match breaker.find(body) {
        Some(nxt) => &body[..nxt.start()],
        None => body,
    };
    Some(cut.to_string())
}

/// The `_first_cycle` DFS: a cycle path among groups (by slug), GRAY/WHITE/
/// BLACK colored. Preconditions: every referenced slug is declared.
fn first_cycle(groups: &[NormalizedGroup]) -> Option<Vec<String>> {
    let adj: std::collections::HashMap<&str, &Vec<String>> = groups
        .iter()
        .map(|g| (g.slug.as_str(), &g.blocked_by_groups))
        .collect();
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }
    let mut color: std::collections::HashMap<&str, Color> =
        adj.keys().map(|s| (*s, Color::White)).collect();
    let mut stack: Vec<&str> = Vec::new();

    fn visit<'a>(
        node: &'a str,
        adj: &std::collections::HashMap<&'a str, &'a Vec<String>>,
        color: &mut std::collections::HashMap<&'a str, Color>,
        stack: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        color.insert(node, Color::Gray);
        stack.push(node);
        if let Some(nexts) = adj.get(node) {
            for nxt in nexts.iter() {
                match color.get(nxt.as_str()) {
                    Some(Color::Gray) => {
                        let idx = stack.iter().position(|s| *s == nxt.as_str())?;
                        let mut path: Vec<String> =
                            stack[idx..].iter().map(|s| (*s).to_string()).collect();
                        path.push(nxt.clone());
                        return Some(path);
                    }
                    Some(Color::White) => {
                        if let Some(found) = visit(nxt, adj, color, stack) {
                            return Some(found);
                        }
                    }
                    _ => {}
                }
            }
        }
        stack.pop();
        color.insert(node, Color::Black);
        None
    }

    let keys: Vec<&str> = adj.keys().copied().collect();
    for s in keys {
        if color.get(s) == Some(&Color::White) {
            if let Some(found) = visit(s, &adj, &mut color, &mut stack) {
                return Some(found);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Spec validation (validate_groups)
// ---------------------------------------------------------------------------

// Validate the group spec before any graph read or write, so the caller
// stays atomic: a bad SPEC fails before the graph is touched.
fn validate_groups(
    groups: &Value,
    max_prs: Option<i64>,
    cap_source: Option<&str>,
    epic_id: Option<&str>,
) -> Result<Vec<NormalizedGroup>, DecomposeErr> {
    if let Some(cap) = max_prs {
        if cap < 1 {
            return Err(DecomposeErr::new(
                format!("--max-prs must be >= 1 (got {cap}); the ceiling cannot be zero"),
                1,
            ));
        }
    }
    let Some(list) = groups.as_array() else {
        return Err(DecomposeErr::new(
            "groups must be a non-empty JSON array of {slug, title, ...} objects",
            1,
        ));
    };
    if list.is_empty() {
        return Err(DecomposeErr::new(
            "groups must be a non-empty JSON array of {slug, title, ...} objects",
            1,
        ));
    }
    if let Some(cap) = max_prs {
        if list.len() > cap as usize {
            let src = cap_source
                .map(String::from)
                .unwrap_or_else(|| format!("--max-prs {cap}"));
            let overflow: Vec<String> = list[cap as usize..]
                .iter()
                .enumerate()
                .map(|(i, g)| group_label(g, cap as usize + i))
                .collect();
            return Err(DecomposeErr::new(
                format!(
                    "{len} groups exceed the cap {src}; fold these {n} into \
                     earlier groups: {overflow} (the cap is a ceiling, not a quota)",
                    len = list.len(),
                    n = overflow.len(),
                    overflow = overflow.join(", "),
                ),
                1,
            ));
        }
    }
    let mut normalized: Vec<NormalizedGroup> = Vec::new();
    let mut seen_slugs: std::collections::BTreeSet<String> = Default::default();
    for (i, grp) in list.iter().enumerate() {
        let Some(obj) = grp.as_object() else {
            return Err(DecomposeErr::new(
                format!("group #{} is not an object", i + 1),
                1,
            ));
        };
        let slug = obj.get("slug").and_then(Value::as_str).unwrap_or("");
        if !group_slug_ok(slug) {
            return Err(DecomposeErr::new(
                format!(
                    "group #{} has invalid slug '{}'; use [A-Za-z0-9-] starting alphanumeric",
                    i + 1,
                    slug
                ),
                1,
            ));
        }
        if seen_slugs.contains(slug) {
            return Err(DecomposeErr::new(
                format!("duplicate group slug '{slug}'"),
                1,
            ));
        }
        seen_slugs.insert(slug.to_string());
        let Some(title) = obj.get("title").and_then(Value::as_str) else {
            return Err(DecomposeErr::new(
                format!("group '{slug}' is missing a non-empty title"),
                1,
            ));
        };
        if title.trim().is_empty() {
            return Err(DecomposeErr::new(
                format!("group '{slug}' is missing a non-empty title"),
                1,
            ));
        }
        let deps: Vec<String> = match obj.get("blocked_by_groups") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                let mut out = Vec::new();
                for d in items {
                    match d.as_str() {
                        Some(s) => out.push(s.to_string()),
                        None => {
                            return Err(DecomposeErr::new(
                                format!("group '{slug}' blocked_by_groups must be a list of slugs"),
                                1,
                            ));
                        }
                    }
                }
                out
            }
            Some(_) => {
                return Err(DecomposeErr::new(
                    format!("group '{slug}' blocked_by_groups must be a list of slugs"),
                    1,
                ));
            }
        };
        let project = match obj.get("project") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
            Some(_) => {
                return Err(DecomposeErr::new(
                    format!("group '{slug}' project must be a non-empty string when set"),
                    1,
                ));
            }
        };
        let cwd = match obj.get("cwd") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
            Some(_) => {
                return Err(DecomposeErr::new(
                    format!("group '{slug}' cwd must be a non-empty string when set"),
                    1,
                ));
            }
        };
        let dep = match obj.get("dep") {
            None => "hard".to_string(),
            Some(Value::String(s)) if s == "hard" || s == "contract" => s.clone(),
            Some(other) => {
                let shown = match other {
                    Value::String(s) => format!("'{s}'"),
                    v => v.to_string(),
                };
                return Err(DecomposeErr::new(
                    format!("group '{slug}' dep must be 'hard' or 'contract' (got {shown})"),
                    1,
                ));
            }
        };
        let stub_against = match obj.get("stub_against") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
            Some(_) => {
                return Err(DecomposeErr::new(
                    format!("group '{slug}' stub_against must be a non-empty string when set"),
                    1,
                ));
            }
        };
        let needs_think = match obj.get("needs_think") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(other) => {
                return Err(DecomposeErr::new(
                    format!("group '{slug}' needs_think must be a boolean (got {other})"),
                    1,
                ));
            }
        };
        // Only an ABSENT adopt key defaults to mint-only; anything present
        // must be a list of non-empty ids (the `or []` form collapses a
        // malformed spec into silently unadopted children).
        let adopt = match obj.get("adopt") {
            None => Vec::new(),
            Some(Value::Array(items)) => {
                let mut out = Vec::new();
                for a in items {
                    match a.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        Some(s) => out.push(s.to_string()),
                        None => {
                            return Err(DecomposeErr::new(
                                format!(
                                    "group '{slug}' adopt must be a list of non-empty \
                                     node ids (got {items:?}); remove the key to adopt nothing"
                                ),
                                1,
                            ));
                        }
                    }
                }
                out
            }
            Some(_) => {
                return Err(DecomposeErr::new(
                    format!(
                        "group '{slug}' adopt must be a list of non-empty node ids; \
                         remove the key to adopt nothing"
                    ),
                    1,
                ));
            }
        };
        normalized.push(NormalizedGroup {
            slug: slug.to_string(),
            title: title.trim().to_string(),
            waves: obj
                .get("waves")
                .map(flex)
                .unwrap_or_default()
                .trim()
                .to_string(),
            blocked_by_groups: deps,
            project,
            cwd,
            dep,
            stub_against,
            needs_think,
            adopt,
        });
    }
    // Adoption re-parents a node under exactly ONE group child, so two
    // groups claiming the same id is ambiguous; name both slugs. The epic
    // cannot adopt itself into its own child (that is a cycle; the CLI
    // re-checks with better wording).
    let mut claimed_by: std::collections::HashMap<String, String> = Default::default();
    for grp in &normalized {
        for aid in &grp.adopt {
            if Some(aid.as_str()) == epic_id {
                return Err(DecomposeErr::new(
                    format!("group '{}' adopt names the epic '{aid}' itself", grp.slug),
                    1,
                ));
            }
            if claimed_by.get(aid) == Some(&grp.slug) {
                return Err(DecomposeErr::new(
                    format!(
                        "group '{}' names node '{aid}' twice in its adopt list",
                        grp.slug
                    ),
                    1,
                ));
            }
            if let Some(prior) = claimed_by.get(aid) {
                return Err(DecomposeErr::new(
                    format!(
                        "node '{aid}' appears in the adopt list of both group \
                         '{prior}' and group '{}'; it can belong to one",
                        grp.slug
                    ),
                    1,
                ));
            }
            claimed_by.insert(aid.clone(), grp.slug.clone());
        }
    }
    // All referenced slugs must be declared in this decomposition, and a
    // `contract` dependent must name the blocker it stubs against.
    for grp in &normalized {
        for dep in &grp.blocked_by_groups {
            if !seen_slugs.contains(dep) {
                return Err(DecomposeErr::new(
                    format!("group '{}' depends on undeclared slug '{dep}'", grp.slug),
                    1,
                ));
            }
        }
        if grp.dep == "contract" && grp.blocked_by_groups.is_empty() {
            return Err(DecomposeErr::new(
                format!(
                    "group '{}' is dep=contract but has no blocked_by_groups; \
                     a contract dependency must name its blocker",
                    grp.slug
                ),
                1,
            ));
        }
    }
    if let Some(cycle) = first_cycle(&normalized) {
        return Err(DecomposeErr::new(
            format!("inter-group dependency cycle: {}", cycle.join(" -> ")),
            2,
        ));
    }
    Ok(normalized)
}

// The overflow label: the group's slug when readable, else its position.
fn group_label(grp: &Value, position: usize) -> String {
    grp.as_object()
        .and_then(|o| o.get("slug"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| format!("#{}", position + 1))
}

// YAML scalars display via str() in the wheel; the same tolerant read here.
fn flex(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The why digest (extract_why_digest) and stub markers
// ---------------------------------------------------------------------------

/// The body under a `## Heading`, bounded by the next `## `/`# ` heading.
fn section_body(doc_text: &str, heading: &regex::Regex) -> String {
    let Some(m) = heading.find(doc_text) else {
        return String::new();
    };
    let body = &doc_text[m.end()..];
    let breaker = regex::Regex::new(r"(?m)^##?[ \t]+").unwrap();
    match breaker.find(body) {
        Some(nxt) => body[..nxt.start()].trim().to_string(),
        None => body.trim().to_string(),
    }
}

/// Transcribe the epic's why for a child scaffold: the epic's intent (the
/// first paragraph of `## Overview`, else the first prose paragraph) plus
/// its `## Locked Decisions` block verbatim. Returns (digest, warning); a
/// why-less epic degrades to the intent line and names the gap, and an
/// unreadable doc yields an empty digest so the caller seeds the stub
/// sentinel instead.
fn extract_why_digest(doc_text: &str) -> (String, Option<String>) {
    use regex::Regex;
    use std::sync::OnceLock;
    static OVERVIEW: OnceLock<Regex> = OnceLock::new();
    static LOCKED: OnceLock<Regex> = OnceLock::new();
    let overview = OVERVIEW.get_or_init(|| Regex::new(r"(?m)^##[ \t]+Overview\b.*$").unwrap());
    let locked = LOCKED.get_or_init(|| Regex::new(r"(?m)^##[ \t]+Locked Decisions\b.*$").unwrap());
    let text = doc_text.replace("\r\n", "\n");
    let mut intent = section_body(&text, overview);
    if !intent.is_empty() {
        if let Some(pos) = intent.find("\n\n") {
            intent = intent[..pos].trim().to_string();
        }
    } else {
        // No Overview heading: first non-heading, non-frontmatter paragraph.
        let front = Regex::new(r"(?s)^---\n.*?\n---\n").unwrap();
        let body = front.replace(&text, "").into_owned();
        for para in Regex::new(r"\n\s*\n").unwrap().split(&body) {
            let para = para.trim();
            if !para.is_empty() && !para.starts_with('#') && !para.starts_with("---") {
                intent = para.to_string();
                break;
            }
        }
    }
    if intent.is_empty() {
        return (String::new(), None);
    }
    let locked_body = section_body(&text, locked);
    if locked_body.is_empty() {
        return (
            intent,
            Some(
                "epic doc has no ## Locked Decisions section; why-digest degraded \
                 to the intent line only - narrow it by hand during inline-fill"
                    .to_string(),
            ),
        );
    }
    (
        format!("{intent}\n\n**Locked Decisions (binding this child):**\n\n{locked_body}"),
        None,
    )
}

// ---------------------------------------------------------------------------
// The epic strategy slice (epic_strategy_from_doc / epic_wave_slice)
// ---------------------------------------------------------------------------

/// A double-quoted YAML scalar via json escaping: the wheel used json.dumps
/// because a hand-escaped quote pair left control characters raw (a task
/// carrying a block-scalar `verify: |` arrived as a string with a real
/// newline and broke the child's Execution Strategy parse).
fn yaml_str(v: &str) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| format!("{v:?}"))
}

/// A wave entry's number, or None when it carries no integral ``wave:``.
fn wave_index(wave: &Value) -> Option<i64> {
    match wave.get("wave") {
        Some(Value::Bool(_)) | None => None,
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        Some(_) => None,
    }
}

fn scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        other => other.to_string(),
    }
}

fn str_list(v: &Value) -> Option<Vec<String>> {
    v.as_array()
        .map(|items| items.iter().map(scalar_to_string).collect::<Vec<String>>())
}

/// parse_execution_strategy's normalization, errors collapsing to None (the
/// caller treats every malformed shape as "the epic doc has no parsable
/// strategy" and reports it by name instead of scaffolding an empty block).
fn parse_strategy_yaml(text: &str) -> Option<Value> {
    let fence = regex::Regex::new(r"(?is)```ya?ml\s*\n(.*?)```").ok()?;
    let raw = match fence.captures(text) {
        Some(c) => c.get(1)?.as_str().to_string(),
        None => text.to_string(),
    };
    let parsed: Value = serde_yaml_ng::from_str(&raw).ok()?;
    if parsed.is_null() {
        return Some(serde_json::json!({"tasks": [], "waves": []}));
    }
    let obj = parsed.as_object()?;
    let tasks_raw = obj.get("tasks").cloned().unwrap_or(Value::Array(vec![]));
    let tasks_arr = tasks_raw.as_array()?;
    let mut tasks: Vec<Value> = Vec::new();
    for t in tasks_arr {
        let t = t.as_object()?;
        let id = scalar_to_string(t.get("id").unwrap_or(&Value::Null));
        // A missing list defaults empty (the wheel's t.get(..., [])); a list
        // PRESENT but malformed still refuses, like the wheel's isinstance check.
        let surface = match t.get("surface") {
            Some(v) => str_list(v)?,
            None => Vec::new(),
        };
        let acceptance = match t.get("acceptance") {
            Some(v) => str_list(v)?,
            None => Vec::new(),
        };
        let blocked_by = match t.get("blocked_by") {
            Some(v) => str_list(v)?,
            None => Vec::new(),
        };
        let notes = scalar_to_string(t.get("notes").unwrap_or(&Value::Null))
            .trim()
            .to_string();
        tasks.push(serde_json::json!({
            "id": id,
            "title": scalar_to_string(t.get("title").unwrap_or(&Value::Null)),
            "surface": surface,
            "verify": scalar_to_string(t.get("verify").unwrap_or(&Value::Null)),
            "acceptance": acceptance,
            "notes": notes,
            "blocked_by": blocked_by,
            "blocked_by_declared": t.contains_key("blocked_by"),
        }));
    }
    let waves = obj
        .get("waves")
        .cloned()
        .unwrap_or_else(|| Value::Array(vec![]));
    if !waves.is_array() {
        return None;
    }
    let execution_mode = obj
        .get("execution_mode")
        .map(scalar_to_string)
        .unwrap_or_else(|| "sequential".to_string());
    Some(serde_json::json!({
        "tasks": tasks,
        "waves": waves,
        "execution_mode": execution_mode,
    }))
}

/// The epic doc's parsed `## Execution Strategy`, or None: scoped to the
/// section body (an epic carries earlier fences that would parse as a
/// strategy with no waves), and None on a missing section or malformed YAML.
fn epic_strategy_from_doc(doc_text: &str) -> Option<Value> {
    let body = execution_strategy_body(doc_text)?;
    parse_strategy_yaml(&body)
}

/// The `## Execution Strategy` body for one group's slice of the epic.
/// Returns `(block, None)` when the epic's parsed strategy yields at least
/// one wave in range, `("", reason)` otherwise: an empty strategy block
/// measures width 0 and reads as a narrow plan rather than a missing one, so
/// the absent case must never emit the heading. Wave numbers and task ids
/// keep the epic's numbering; a `blocked_by` naming a task outside the slice
/// (or one with no task row) is dropped - that blocker ships in a sibling
/// child's PR, and a dangling id would make the child unmeasurable.
fn epic_wave_slice(epic_strategy: Option<&Value>, waves_spec: &str) -> (String, Option<String>) {
    let Some(strategy) = epic_strategy.filter(|s| !s.is_null()) else {
        return (
            String::new(),
            Some("the epic doc has no parsable execution strategy section".to_string()),
        );
    };
    let wanted = wave_numbers(waves_spec);
    if wanted.is_empty() {
        return (
            String::new(),
            Some(format!(
                "the group's wave range '{}' names no wave number",
                waves_spec
            )),
        );
    }
    let empty = Vec::new();
    let waves_list = strategy
        .get("waves")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let waves: Vec<&Value> = waves_list
        .iter()
        .filter(|w| {
            w.is_object()
                && wave_index(w)
                    .map(|n| wanted.contains(&(n as u32)))
                    .unwrap_or(false)
        })
        .collect();
    if waves.is_empty() {
        return (
            String::new(),
            Some(format!(
                "the epic's execution strategy declares no wave in range {}",
                waves_spec
            )),
        );
    }
    let mut sliced_ids: Vec<String> = Vec::new();
    for w in &waves {
        if let Some(ids) = w.get("tasks").and_then(Value::as_array) {
            for tid in ids {
                let s = scalar_to_string(tid);
                if !sliced_ids.contains(&s) {
                    sliced_ids.push(s);
                }
            }
        }
    }
    let empty_tasks = Vec::new();
    let tasks_arr = strategy
        .get("tasks")
        .and_then(Value::as_array)
        .unwrap_or(&empty_tasks);
    let by_id: std::collections::HashMap<&str, &Value> = tasks_arr
        .iter()
        .filter_map(|t| t.get("id").and_then(Value::as_str).map(|id| (id, t)))
        .collect();

    let mut lines: Vec<String> = vec![
        "```yaml".to_string(),
        format!(
            "execution_mode: {}",
            strategy
                .get("execution_mode")
                .and_then(Value::as_str)
                .unwrap_or("sequential")
        ),
        "waves:".to_string(),
    ];
    for w in &waves {
        let idx = wave_index(w).unwrap_or(0);
        lines.push(format!("  - wave: {idx}"));
        let mode = w
            .get("mode")
            .map(scalar_to_string)
            .unwrap_or_else(|| "sequential".to_string());
        lines.push(format!("    mode: {mode}"));
        if let Some(name) = w.get("name").filter(|n| !n.is_null()) {
            lines.push(format!("    name: {}", yaml_str(&scalar_to_string(name))));
        }
        // The child is stamped `created: <today>`, so it is post-difficulty-gate
        // and every wave needs a band; an epic wave without one falls back to
        // the same floor band the scaffold's own frontmatter carries.
        let difficulty = w
            .get("difficulty")
            .map(scalar_to_string)
            .unwrap_or_else(|| "low".to_string());
        lines.push(format!("    difficulty: {difficulty}"));
        // Only ids that resolve to a real task row; naming a dangling id here
        // would mint a child that fails its own `wave references unknown task`
        // check on a doc nobody mis-authored.
        let ids: Vec<String> = w
            .get("tasks")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(scalar_to_string)
                    .filter(|s| by_id.contains_key(s.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let quoted: Vec<String> = ids.iter().map(|i| format!("'{i}'")).collect();
        lines.push(format!("    tasks: [{}]", quoted.join(", ")));
    }
    lines.push("tasks:".to_string());
    for tid in &sliced_ids {
        let Some(t) = by_id.get(tid.as_str()) else {
            continue;
        };
        lines.push(format!("  - id: '{tid}'"));
        let title = t
            .get("title")
            .map(scalar_to_string)
            .unwrap_or_else(|| tid.clone());
        lines.push(format!("    title: {}", yaml_str(&title)));
        let surface: Vec<String> = t
            .get("surface")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(scalar_to_string)
                    .filter(|s| !s.trim().is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let quoted_surface: Vec<String> = surface.iter().map(|s| yaml_str(s)).collect();
        lines.push(format!("    surface: [{}]", quoted_surface.join(", ")));
        let verify = t.get("verify").map(scalar_to_string).unwrap_or_default();
        if !verify.is_empty() {
            lines.push(format!("    verify: {}", yaml_str(&verify)));
        }
        let acceptance: Vec<String> = t
            .get("acceptance")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(scalar_to_string)
                    .filter(|a| !a.trim().is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if !acceptance.is_empty() {
            lines.push("    acceptance:".to_string());
            for a in &acceptance {
                lines.push(format!("      - {}", yaml_str(a)));
            }
        }
        let declared = t
            .get("blocked_by_declared")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if declared {
            let kept: Vec<String> = t
                .get("blocked_by")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(scalar_to_string)
                        .filter(|b| sliced_ids.contains(b) && by_id.contains_key(b.as_str()))
                        .collect()
                })
                .unwrap_or_default();
            let quoted_kept: Vec<String> = kept.iter().map(|b| format!("'{b}'")).collect();
            lines.push(format!("    blocked_by: [{}]", quoted_kept.join(", ")));
        }
    }
    lines.push("```".to_string());
    (lines.join("\n"), None)
}

// ---------------------------------------------------------------------------
// The child plan path and the scaffold writer
// ---------------------------------------------------------------------------

/// The canonical `fno do plan path` for a group child's scaffold stub: the
/// `<child-project plans dir>/YYYYMMDD-<slug>-<id>. created_at (not today)
/// sources the filename date, which is what makes a re-decompose on a later
/// day idempotent. An unparseable/absent created_at degrades to today with a
/// stderr warning (defensive: mint always writes ISO+00:00, so this is a
/// hand-corruption path only).
fn canonical_child_plan_path(
    slug: &str,
    child_id: &str,
    child_root: &str,
    created_at: Option<&str>,
) -> String {
    let mut at: Option<PinnedTimestamp> = None;
    if let Some(raw) = created_at {
        if !raw.trim().is_empty() {
            match chrono::DateTime::parse_from_rfc3339(raw) {
                Ok(dt) => {
                    let secs = dt.with_timezone(&chrono::Utc).timestamp();
                    at = PinnedTimestamp::from_epoch(secs);
                }
                Err(_) => eprintln!(
                    "warning: child {child_id} has unparseable created_at \
                     {raw:?}; using today's date for its plan filename"
                ),
            }
        }
    }
    crate::plans_path::plan_doc_path(Path::new(child_root), slug, child_id, at)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|e| {
            eprintln!("warning: could not resolve the plans dir for {child_root}: {e}");
            String::new()
        })
}

/// The seed the scaffold leaves in `## Why (from epic)` when the epic doc
/// yields no usable intent - itself a stub marker, so the validator refuses a
/// child whose why never got filled.
const WHY_STUB: &str = "<!-- Why (from epic): transcribe the epic's intent line + the Locked \
     Decisions that bind this child (not a pointer). -->";

/// One per-group scaffold stub for one group child: born `status: idea`
/// (never `ready`), seeded from the group's wave range, a transcribed
/// `## Why (from epic)`, stub markers for the concrete detail the builder
/// fills inline, and the epic's own Execution-Strategy slice when its wave
/// range names one. The epic doc stays the design authority; the child
/// carries its own execution plan.
fn scaffold_separate_plan(
    group: &NormalizedGroup,
    epic_id: &str,
    source_doc: &str,
    why_digest: &str,
    adopted: &[(String, String)],
    created: Option<&str>,
    epic_strategy: Option<&Value>,
) -> String {
    let yaml_title = group.title.replace('\\', "\\\\").replace('"', "\\\"");
    // A scaffold with no `created:` is invisible to the consolidation gate
    // (which reads that key to tell a new plan from a pre-gate one), so the
    // same stamp also makes the child post-gate for the difficulty gate.
    let created_date = created
        .map(String::from)
        .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());
    let why_block = if why_digest.trim().is_empty() {
        WHY_STUB
    } else {
        why_digest
    };
    let coverage_block = if adopted.is_empty() {
        String::new()
    } else {
        let checklist: String = adopted
            .iter()
            .map(|(nid, title)| format!("- [ ] `{nid}` - {title}\n"))
            .collect();
        format!(
            "## Adopted coverage\n\nThis PR folds work that already had its own node; \
             both close on merge whether or not the plan below addresses them, so \
             check each off explicitly.\n\n{checklist}\n"
        )
    };
    let (slice_yaml, slice_why) = epic_wave_slice(epic_strategy, &group.waves);
    let strategy_block = if slice_yaml.is_empty() {
        format!(
            "\n<!-- No execution strategy carried from the epic: {}. Write one \
             here, or this child measures no width and join refuses it. -->\n",
            slice_why.unwrap_or_default()
        )
    } else {
        format!("\n## Execution Strategy\n\n{slice_yaml}\n")
    };
    format!(
        "---\ntitle: \"{yaml_title}\"\nstatus: idea\nkind: quick-plan\ncreated: \
         {created_date}\ndifficulty: low\nconsolidation:\n  # <!-- Consolidation: run the \
         step 2d gate against this child and record one outcome with a reason per id \
         considered. -->\n  outcome: proceed_alone\n  proceed_alone_against: []\nparent_epic: \
         {epic_id}\nsource_doc: {source_doc}\n---\n\n# {title}\n\n## Why (from epic)\n\n\
         {why_block}\n\n## Context\n\nGroup child of epic `{epic_id}` (see `{source_doc}`). \
         Covers wave(s) {waves} of the epic's Execution Strategy. This is a self-contained \
         quick-plan for a fresh-context builder; pull scope detail from the named waves and \
         the epic's `## File Ownership Map`.\n\n{coverage_block}## Changes\n\n<!-- Seeded \
         from epic waves {waves}. Fill in the concrete changes this group ships. -->\n\n\
         ## Files to Modify\n\n<!-- From the epic's File Ownership Map: the files this \
         group owns. -->\n\n## Verification\n\n<!-- The checks that prove this group's \
         slice works. -->\n{strategy_block}",
        title = group.title,
        waves = if group.waves.is_empty() {
            "(unset)"
        } else {
            group.waves.as_str()
        },
    )
}

// ---------------------------------------------------------------------------
// The containment guards (the _contain.py twin, decompose's wording)
// ---------------------------------------------------------------------------

/// True when a node presents as done: a completed_at that is not the legacy
/// defer spelling. The wheel's `statuses.node_is_done` twin.
fn node_is_done(node: &Value) -> bool {
    let completed = match node.get("completed_at").and_then(Value::as_str) {
        Some(s) => s,
        None => return false,
    };
    !completed.is_empty() && !completed.starts_with("deferred:")
}

/// Refuse a deferred or superseded owner: a dead delivery unit cannot own
/// containment. Returns Some(error) to raise, decompose's wording with
/// `context` as the caller's noun phrase.
fn refuse_dead_owner(owner: &Value, context: &str) -> Option<DecomposeErr> {
    let owner_id = text_at(owner, "id").unwrap_or("");
    let completed = text_at(owner, "completed_at").unwrap_or("");
    let legacy_defer = completed.starts_with("deferred:");
    // The wheel's shape: `not (bool(completed) and not legacy_defer)` on the
    // left, truthy deferred_at / superseded_by / legacy_defer on the right.
    let has_completed = !completed.is_empty();
    let truthy = |key: &str| -> bool {
        owner
            .get(key)
            .map(|v| !v.is_null() && !v.as_str().unwrap_or("").is_empty())
            .unwrap_or(false)
    };
    let dead = (!has_completed || legacy_defer)
        && (truthy("deferred_at") || truthy("superseded_by") || legacy_defer);
    if !dead {
        return None;
    }
    let (how, containment, remedy) = match text_at(owner, "superseded_by") {
        Some(superseder) => (
            format!("was superseded by {superseder}"),
            "its death already released the nodes it contained and nothing re-runs \
             that release"
                .to_string(),
            format!(
                "Run `fno backlog unsupersede {owner_id}` to revive it (clears \
                 superseded_by; `undefer` does not), or point the adopt list at the \
                 superseding node, or give the group a new slug so it mints a live \
                 delivery unit"
            ),
        ),
        None => (
            "is deferred".to_string(),
            "its contained nodes remain folded under it (defer keeps containment; \
             nothing re-runs a release)"
                .to_string(),
            format!(
                "Run `fno backlog undefer {owner_id}` first (its children resume \
                 contained, not released), or drop the adopt list from this group"
            ),
        ),
    };
    Some(DecomposeErr::new(
        format!(
            "{context} resolves to {owner_id}, which {how}; {containment}, so stamping \
             containment here would leave every adoptee undispatchable with no verb to \
             free it. {remedy}"
        ),
        2,
    ))
}

/// One target's adoption result: `adopted` is whether the parent pointer
/// moved this call; `warning` is the short reason containment was withheld
/// (a done node with its own PR or cost), for the caller to render.
pub(crate) struct ContainOutcome {
    pub adopted: bool,
    pub warning: Option<String>,
}

/// Fold the target at `target_idx` into `owner_id`: guards, then stamp
/// `contained_in` + `parent`. Guard order is load-bearing: live worker, cycle,
/// one-level children, mid-flight delivery unit (refuse), then the
/// withheld-containment case for a done node with its own PR or cost. Callers
/// hold the graph lock.
fn contain_into(
    entries: &mut [Value],
    owner_id: &str,
    target_idx: usize,
    context: &str,
) -> Result<ContainOutcome, DecomposeErr> {
    let target_id = text_at(&entries[target_idx], "id")
        .unwrap_or_default()
        .to_string();
    if let Some(holder) = super::create_cli::live_worker(&target_id) {
        return Err(DecomposeErr::new(
            format!(
                "{context} adopts {target_id}, which is being built right now by \
                 {holder}; adopting it would leave that session holding a claim on a \
                 node that no longer dispatches, and it would still open its own PR. \
                 Wait for it to land, or stop it first"
            ),
            2,
        ));
    }
    if crate::backlog::node_ref::would_create_cycle(entries, &target_id, owner_id) {
        return Err(DecomposeErr::new(
            format!("adopting {target_id} into {context} would create a cycle"),
            2,
        ));
    }
    let kids: Vec<String> = entries
        .iter()
        .filter(|e| text_at(e, "parent") == Some(target_id.as_str()))
        .filter_map(|e| text_at(e, "id"))
        .map(String::from)
        .collect();
    if !kids.is_empty() {
        let shown: Vec<&str> = kids.iter().take(3).map(String::as_str).collect();
        let ellipsis = if kids.len() > 3 { "..." } else { "" };
        return Err(DecomposeErr::new(
            format!(
                "{context} adopts {target_id}, which has {} child(ren) ({}{}); \
                 containment is one level, so they would stay dispatchable and open \
                 their own PRs while their parent closed. Adopt the children \
                 individually, or re-parent them out first",
                kids.len(),
                shown.join(", "),
                ellipsis
            ),
            2,
        ));
    }
    let target = &entries[target_idx];
    let own_pr = target.get("pr_number").and_then(Value::as_i64);
    let own_cost = target.get("cost_usd").and_then(Value::as_f64);
    let has_own = own_pr.is_some() || own_cost.is_some();
    if has_own && !node_is_done(target) {
        let what = if own_pr.is_some() {
            format!("has an open PR (#{})", own_pr.unwrap())
        } else {
            "has accrued cost".to_string()
        };
        return Err(DecomposeErr::new(
            format!(
                "{context} adopts {target_id}, which {what} and has not landed; it is \
                 its own delivery unit mid-flight. Adopting it would hang open work \
                 under the group, and the epic would close over it when the group \
                 merges. Let it land first, or drop it from the adopt list"
            ),
            2,
        ));
    }
    let mut warning: Option<String> = None;
    if has_own {
        warning = Some(if own_pr.is_some() {
            format!("carries PR #{}", own_pr.unwrap())
        } else {
            "carries cost".to_string()
        });
    } else {
        let obj = entries[target_idx]
            .as_object_mut()
            .expect("row is an object");
        obj.insert("contained_in".into(), Value::String(owner_id.to_string()));
        if let Some(o) = obj.get_mut("released_from") {
            o.take();
        }
    }
    if text_at(&entries[target_idx], "parent") == Some(owner_id) {
        // already adopted: the parent pointer moved on a prior run
        return Ok(ContainOutcome {
            adopted: false,
            warning,
        });
    }
    let obj = entries[target_idx]
        .as_object_mut()
        .expect("row is an object");
    obj.insert("parent".into(), Value::String(owner_id.to_string()));
    Ok(ContainOutcome {
        adopted: true,
        warning,
    })
}

// ---------------------------------------------------------------------------
// The CLI layer (run_decompose)
// ---------------------------------------------------------------------------

const DECOMPOSE_USAGE: &str =
    "Usage: fno backlog decompose <epic> --groups <json|@file|-> [--max-prs N] [--force]";

/// Classify the epic frontmatter's `max_children` the way the wheel's
/// `_UNSET` sentinel does: absent key reads Unset; a present key must parse
/// as a positive integer or it fails closed as Invalid.
fn max_children_from_frontmatter(fm: &serde_json::Map<String, Value>) -> MaxChildren {
    let Some(v) = fm.get("max_children") else {
        return MaxChildren::Unset;
    };
    match v {
        Value::Number(n) => match n.as_i64() {
            Some(i) => MaxChildren::Value(i),
            None => MaxChildren::Invalid(v.to_string()),
        },
        Value::String(s) => match s.trim().parse::<i64>() {
            Ok(i) => MaxChildren::Value(i),
            Err(_) => MaxChildren::Invalid(format!("{s:?}")),
        },
        other => MaxChildren::Invalid(other.to_string()),
    }
}

/// The create guard, shared spelling with intake's door.
fn refuse_external_backend_for_decompose() -> bool {
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

/// `fno backlog decompose <epic> ...`: the ported cmd_decompose. Pre-lock
/// validation, one locked graph mutation for the whole decomposition, then
/// the post-lock scaffolds / fan-out / receipts.
pub fn run_decompose(tail: &[String]) -> i32 {
    if tail.iter().any(|a| a == "--help" || a == "-h") {
        println!("{DECOMPOSE_USAGE}");
        println!();
        println!("Upsert group child nodes under an epic (atomic + idempotent).");
        println!("Each group becomes one child node (parent=epic) bundling 1+ execution");
        println!("waves into a single shippable PR, with its own self-contained");
        println!("<stem>.group-<slug>.md quick-plan (the only packaging). Re-running with");
        println!("the same slugs updates the existing children in place rather than");
        println!("duplicating, keyed on the slug.");
        return 0;
    }
    let mut epic_arg: Option<String> = None;
    let mut groups_src: Option<String> = None;
    let mut max_prs: Option<i64> = None;
    let mut force = false;
    let mut json_mode = false;
    let mut i = 0usize;
    while i < tail.len() {
        let tok = tail[i].as_str();
        match tok {
            "--groups" | "-g" => match tail.get(i + 1) {
                Some(v) => {
                    groups_src = Some(v.clone());
                    i += 1;
                }
                None => {
                    eprintln!("Error: --groups needs a value (JSON, @file, or - for stdin)");
                    return 2;
                }
            },
            "--max-prs" | "-m" => match tail.get(i + 1).and_then(|v| v.parse::<i64>().ok()) {
                Some(n) => {
                    max_prs = Some(n);
                    i += 1;
                }
                None => {
                    eprintln!("Error: --max-prs needs an integer");
                    return 2;
                }
            },
            "--force" | "-F" => force = true,
            "--json" | "-J" => json_mode = true,
            other => {
                if other.starts_with('-') && other != "-" && other.len() > 1 {
                    eprintln!("Error: unknown flag {other}");
                    return 2;
                }
                if epic_arg.is_none() {
                    epic_arg = Some(other.to_string());
                } else {
                    eprintln!("Error: unexpected extra argument {other}");
                    return 2;
                }
            }
        }
        i += 1;
    }
    if tail.iter().any(|a| a == "--plans") {
        // The packaging option was removed wheel-side with the same message
        // the wheel prints before any work; `separate` is the only packaging.
        eprintln!("Error: --plans fragment was removed; 'separate' is now the only packaging (one plan == one PR == one node). Drop the flag or pass --plans separate.");
        return 1;
    }
    let Some(epic_arg) = epic_arg else {
        eprintln!("{DECOMPOSE_USAGE}");
        eprintln!("Error: an epic node id is required");
        return 2;
    };
    let Some(groups_src) = groups_src else {
        eprintln!("Error: --groups is required (JSON, @file, or - for stdin)");
        return 2;
    };
    if refuse_external_backend_for_decompose() {
        return 1;
    }
    // 1. Read the --groups source, keeping read vs parse failures distinct.
    let raw = if groups_src == "-" {
        let mut buf = String::new();
        use std::io::Read;
        if std::io::stdin().read_to_string(&mut buf).is_err() {
            eprintln!("Error: could not read --groups from stdin");
            return 1;
        }
        buf
    } else if let Some(path) = groups_src.strip_prefix('@') {
        let expanded = super::intake::expand_user(path);
        match std::fs::read_to_string(&expanded) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Error: could not read --groups file {path:?}: {e}");
                return 1;
            }
        }
    } else {
        groups_src.clone()
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error: --groups is not valid JSON: {e}");
            return 1;
        }
    };
    let graph = super::settings::graph_path();

    // 2. Resolve the ceiling. The epic's frontmatter cap is read fail-safe
    //    pre-lock; config is read ONLY on the true fallback path.
    let mut max_children = MaxChildren::Unset;
    let mut epic_doc_rel: Option<String> = None;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Ok(rows) = crate::graph_store::read_rows(&graph) {
        if let Some(epic_node) = crate::backlog::node_ref::find_node(&rows, &epic_arg) {
            if let Some(pp) = text_at(epic_node, "plan_path").filter(|p| !p.is_empty()) {
                let epic_doc = plan_base(Some(pp)).ok();
                if let Some(doc) = epic_doc {
                    let abs = if Path::new(&doc).is_absolute() {
                        doc.clone()
                    } else {
                        Path::new(text_at(epic_node, "cwd").unwrap_or("."))
                            .join(&doc)
                            .to_string_lossy()
                            .into_owned()
                    };
                    let fm = super::intake::read_plan_frontmatter(Path::new(&abs));
                    max_children = max_children_from_frontmatter(&fm);
                    epic_doc_rel = Path::new(&abs)
                        .strip_prefix(
                            Path::new(text_at(epic_node, "cwd").unwrap_or(".")).to_path_buf(),
                        )
                        .ok()
                        .map(|r| r.to_string_lossy().into_owned())
                        .or_else(|| {
                            Path::new(&abs)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                        });
                }
            }
        }
    }
    let mut config_default: Option<i64> = None;
    if matches!(max_children, MaxChildren::Unset) && max_prs.is_none() {
        match crate::agents_config::config_lookup(&cwd, &["blueprint", "max_prs_per_epic"]) {
            Some(v) => match v.as_integer() {
                Some(n) if n >= 1 => config_default = Some(n),
                _ => {
                    eprintln!("Error: could not read config.blueprint.max_prs_per_epic: it must be an integer >= 1");
                    return 1;
                }
            },
            None => config_default = Some(4),
        }
    }
    let (effective_cap, cap_source) = match resolve_effective_cap(
        &max_children,
        max_prs,
        config_default,
        epic_doc_rel.as_deref(),
    ) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("Error: {}", e.message);
            return e.exit_code;
        }
    };
    // 3. Validate the spec entirely before touching the graph (atomicity).
    let norm = match validate_groups(
        &parsed,
        Some(effective_cap),
        Some(&cap_source),
        Some(epic_arg.as_str()),
    ) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("Error: {}", e.message);
            return e.exit_code;
        }
    };
    // 3b. Per-group repo routing OUTSIDE the lock: an explicit cwd is used
    //     as-is; a project alone must resolve through the work-map or the
    //     whole request refuses before any write.
    let mut slug_route: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    for grp in &norm {
        match (&grp.cwd, &grp.project) {
            (Some(gcwd), _) => {
                let abs = super::intake::abspath_cwd(
                    gcwd,
                    &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                );
                slug_route.push((grp.slug.clone(), grp.project.clone(), Some(abs)));
            }
            (None, Some(gproj)) => match super::intake::project_root_from_settings(gproj) {
                Some(root) => slug_route.push((grp.slug.clone(), Some(gproj.clone()), Some(root))),
                None => {
                    eprintln!(
                        "Error: group '{}' project {gproj:?} is not in any \
                             settings.yaml work-map; add it there or pass an explicit cwd",
                        grp.slug
                    );
                    return 1;
                }
            },
            (None, None) => slug_route.push((grp.slug.clone(), None, None)),
        }
    }
    let route_of = |slug: &str| -> (Option<String>, Option<String>) {
        slug_route
            .iter()
            .find(|(s, _, _)| s == slug)
            .map(|(_, p, c)| (p.clone(), c.clone()))
            .unwrap_or((None, None))
    };
    let keep_slugs: std::collections::BTreeSet<String> =
        norm.iter().map(|g| g.slug.clone()).collect();
    decompose_commit(
        &graph,
        norm,
        route_of,
        &keep_slugs,
        &epic_arg,
        effective_cap,
        max_prs,
        force,
        json_mode,
    )
}

// ---------------------------------------------------------------------------
// The locked mutation + post-lock steps
// ---------------------------------------------------------------------------

/// What the locked mutator hands back for the post-lock steps: the resolved
/// epic id, per-group receipt rows, and the boxes the human receipt prints.
struct MutateOut {
    epic_id: String,
    base: String,
    base_abs: Option<String>,
    epic_cwd: Option<String>,
    results: Vec<Value>,
    orphan_ids: Vec<String>,
    unadopted_ids: Vec<String>,
    contained_unadopted: Vec<String>,
    uncontained: Vec<String>,
    downgrades: Vec<String>,
}

fn child_plan_path(base: &str, slug: &str) -> String {
    format!("{base}#group-{slug}")
}

/// One locked graph mutation for the whole decomposition, retried on a store
/// conflict. A refused spec writes nothing; every refusal names its class
/// (exit 1 user error, 2 bad state, 3 not found).
fn decompose_commit(
    graph: &Path,
    norm: Vec<NormalizedGroup>,
    route_of: impl Fn(&str) -> (Option<String>, Option<String>),
    keep_slugs: &std::collections::BTreeSet<String>,
    epic_arg: &str,
    _effective_cap: i64,
    _max_prs: Option<i64>,
    force: bool,
    json_mode: bool,
) -> i32 {
    let mut out: Option<MutateOut> = None;
    for retry in 0..6 {
        let rows = match crate::graph_store::read_rows(graph) {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("Error: the graph store is unreadable: {e}");
                return 1;
            }
        };
        let base_version = match crate::graph_store::base_version(graph) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Error: the graph store is unreadable: {e}");
                return 1;
            }
        };
        let mut working = rows;
        let outcome =
            match decompose_mutate(&mut working, &norm, &route_of, keep_slugs, epic_arg, force) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("Error: {}", e.message);
                    return e.exit_code;
                }
            };
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
                out = Some(outcome);
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
    let Some(out) = out else {
        return 1;
    };
    decompose_post_steps(graph, out, norm, json_mode)
}

/// The locked mutator: epic resolve, cap-independent pre-checks, Pass 1
/// upsert + adoption, Pass 2 titles/edges/dep tiers, orphan + unadopted
/// scans. Any Err aborts the write with the graph untouched.
fn decompose_mutate(
    entries: &mut Vec<Value>,
    norm: &[NormalizedGroup],
    route_of: &impl Fn(&str) -> (Option<String>, Option<String>),
    keep_slugs: &std::collections::BTreeSet<String>,
    epic_arg: &str,
    force: bool,
) -> Result<MutateOut, DecomposeErr> {
    let epic_idx = crate::backlog::node_ref::find_node_index(entries, epic_arg)
        .ok_or_else(|| DecomposeErr::new(format!("epic node {epic_arg} not found"), 3))?;
    let epic_node = &entries[epic_idx];
    let live_priority = text_at(epic_node, "priority").unwrap_or("p2").to_string();
    let blocks_everything = epic_node
        .get("blocks_everything")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Err(r) = super::create_cli::validate_priority(&live_priority, blocks_everything) {
        return Err(DecomposeErr::new(r.message, 2));
    }
    let epic_id = text_at(&entries[epic_idx], "id")
        .unwrap_or(epic_arg)
        .to_string();
    let base = plan_base(text_at(&entries[epic_idx], "plan_path"))?;
    let base_abs: Option<String> = if Path::new(&base).is_absolute() {
        Some(base.clone())
    } else {
        Some(
            Path::new(text_at(&entries[epic_idx], "cwd").unwrap_or("."))
                .join(&base)
                .to_string_lossy()
                .into_owned(),
        )
    };
    let epic_cwd = text_at(&entries[epic_idx], "cwd").map(String::from);
    // The doc is the single source of truth for a `contract` tier: no
    // readable doc means no pin, and every contract request falls back to
    // hard (reported, never silent).
    let pinned_versions: std::collections::BTreeSet<u32> = base_abs
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| extract_contract_versions(&t))
        .unwrap_or_default();
    // Refuse to orphan an already-shipped group child unless --force.
    let orphans = find_orphans(entries, &epic_id, &base, keep_slugs);
    let shipped_orphans: Vec<String> = orphans
        .iter()
        .filter(|o| is_shipped(o))
        .filter_map(|o| text_at(o, "id"))
        .map(String::from)
        .collect();
    if !shipped_orphans.is_empty() && !force {
        return Err(DecomposeErr::new(
            format!(
                "re-decomposition would orphan already-shipped group node(s): {}. \
                 Re-run with --force to proceed, or keep their #group slugs.",
                shipped_orphans.join(", ")
            ),
            2,
        ));
    }
    // Pass 1: resolve each group to an existing or new child node.
    let mut slug_to_id: std::collections::BTreeMap<String, String> = Default::default();
    let mut adopt_claim: std::collections::BTreeMap<String, String> = Default::default();
    let mut plan_to_group: Vec<(usize, usize)> = Vec::new();
    let mut results: Vec<Value> = Vec::new();
    let mut uncontained: Vec<String> = Vec::new();
    for (gi, grp) in norm.iter().enumerate() {
        let frag_path = child_plan_path(&base, &grp.slug);
        let sep_path = separate_plan_path(&base, &grp.slug);
        // Tolerant lookup: identity is the durable group_slug, so a child
        // born unlinked is still found; the legacy plan_path shapes upsert
        // a pre-field child in place instead of duplicating.
        let existing = entries.iter().position(|e| {
            text_at(e, "parent") == Some(epic_id.as_str())
                && (text_at(e, "group_slug") == Some(grp.slug.as_str())
                    || text_at(e, "plan_path") == Some(frag_path.as_str())
                    || text_at(e, "plan_path") == Some(sep_path.as_str()))
        });
        let (node_idx, action) = match existing {
            Some(idx) => {
                // Preserve a designed child's plan_path; the one exception is
                // the documented legacy-fragment repoint. An explicit route
                // reprojects the child's repo.
                let (route_proj, route_cwd) = route_of(&grp.slug);
                let node = &mut entries[idx];
                if let Some(obj) = node.as_object_mut() {
                    obj.insert("group_slug".into(), Value::String(grp.slug.clone()));
                    if obj.get("plan_path").and_then(Value::as_str) == Some(frag_path.as_str()) {
                        obj.insert("plan_path".into(), Value::String(sep_path.clone()));
                    }
                    if let Some(p) = route_proj {
                        obj.insert("project".into(), Value::String(p));
                    }
                    if let Some(c) = route_cwd {
                        obj.insert("cwd".into(), Value::String(c));
                    }
                }
                (idx, "updated")
            }
            None => {
                // Born UNLINKED: linking the filled plan is the
                // design-completion signal that flips the child ready.
                let (route_proj, route_cwd) = route_of(&grp.slug);
                let epic = &entries[epic_idx];
                let project = route_proj.or_else(|| text_at(epic, "project").map(String::from));
                let node_cwd = route_cwd.or_else(|| text_at(epic, "cwd").map(String::from));
                let priority = text_at(epic, "priority").unwrap_or("p2").to_string();
                let blocks = epic
                    .get("blocks_everything")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let difficulty = text_at(epic, "difficulty").map(String::from);
                let domain = text_at(epic, "domain").unwrap_or("code").to_string();
                let known: std::collections::BTreeSet<String> = entries
                    .iter()
                    .filter_map(|e| text_at(e, "id"))
                    .map(String::from)
                    .collect();
                let now = crate::graph_store::now_isoformat();
                // Built before the big json! literal: a closure returning an
                // array of json objects inside a json! value position is
                // handed back as raw Rust tokens, and `{ "value": ... }`
                // there is a block, not a json object.
                let difficulty_history: Value = difficulty
                    .as_ref()
                    .map(|d| {
                        serde_json::json!([
                            {"value": d, "source": "filed", "ts": now.clone()}
                        ])
                    })
                    .unwrap_or_else(|| Value::Array(vec![]));
                let (origin, origin_evidence_ref, _refused) =
                    super::create_cli::stamp_request_origin(
                        "organic",
                        "decompose",
                        Some(&format!("parent:{epic_id}")),
                    );
                let prov = super::create_cli::session_provenance(
                    &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                    None,
                    Some(&known),
                );
                let node_id =
                    super::create_cli::mint_node_id(&known).map_err(DecomposeErr::from)?;
                if crate::backlog::node_ref::would_create_cycle(entries, &node_id, &epic_id) {
                    return Err(DecomposeErr::new(
                        format!(
                            "parenting group '{}' to {epic_id} would create a cycle",
                            grp.slug
                        ),
                        2,
                    ));
                }
                let node = serde_json::json!({
                    "id": node_id,
                    "parent": epic_id,
                    "tags": [],
                    "title": grp.title,
                    "type": "feature",
                    "project": project,
                    "cwd": node_cwd,
                    "priority": priority,
                    "blocks_everything": blocks,
                    "difficulty": difficulty,
                    "difficulty_history": difficulty_history,
                    "domain": domain,
                    "blocked_by": [],
                    "session_id": null,
                    "locked_at": null,
                    "completed_at": null,
                    "has_brief": false,
                    "roadmap_id": null,
                    "vision_path": null,
                    "details": null,
                    "size": null,
                    "batch": null,
                    "cost_usd": null,
                    "cost_sessions": [],
                    "plan_path": null,
                    "pr_number": null,
                    "pr_url": null,
                    "merge_status": null,
                    "created_at": now,
                    "source": null,
                    "source_kind": "organic",
                    "source_project": null,
                    "source_inbox_msg": null,
                    "artifact_url": null,
                    "completion_note": null,
                    "source_session_id": prov.0,
                    "source_harness": prov.1,
                    "source_cwd": prov.2,
                    "source_node_id": prov.3,
                    "source_plan_path": prov.4,
                    "request_origin": origin,
                    "origin_evidence": origin_evidence_ref,
                    "group_slug": grp.slug,
                });
                entries.push(node);
                (entries.len() - 1, "created")
            }
        };
        slug_to_id.insert(
            grp.slug.clone(),
            text_at(&entries[node_idx], "id").unwrap_or("").to_string(),
        );
        plan_to_group.push((node_idx, gi));

        // Adoption (US2): re-parent each named node under this group child,
        // inside the same locked mutation. Nothing is minted for it and
        // nothing is deleted; membership rides the parent pointer.
        let mut adopted: Vec<String> = Vec::new();
        let owner_id = text_at(&entries[node_idx], "id")
            .unwrap_or_default()
            .to_string();
        if !grp.adopt.is_empty() {
            if let Some(err) =
                refuse_dead_owner(&entries[node_idx], &format!("group '{}'", grp.slug))
            {
                return Err(err);
            }
        }
        for adopt_id in &grp.adopt {
            let target_idx = crate::backlog::node_ref::find_node_index(entries, adopt_id)
                .ok_or_else(|| {
                    DecomposeErr::new(
                        format!(
                            "group '{}' adopts {adopt_id}, which resolves to no node",
                            grp.slug
                        ),
                        3,
                    )
                })?;
            let target_id = text_at(&entries[target_idx], "id")
                .unwrap_or(adopt_id)
                .to_string();
            if target_id == epic_id {
                return Err(DecomposeErr::new(
                    format!("group '{}' adopt names the epic {epic_id} itself", grp.slug),
                    1,
                ));
            }
            if let Some(prior) = adopt_claim.get(&target_id) {
                return Err(DecomposeErr::new(
                    format!(
                        "node {target_id} is claimed by more than one adopt entry \
                         (group '{prior}' and group '{}'); two spellings of one id \
                         resolve to the same node",
                        grp.slug
                    ),
                    1,
                ));
            }
            adopt_claim.insert(target_id.clone(), grp.slug.clone());
            // Unscoped on purpose: group_child_slug answers "group child of
            // THIS doc", which misses a legacy child of ANOTHER epic and lets
            // adoption steal it. Also covers self-adoption.
            if is_group_child(&entries[target_idx]) {
                let whose = match group_child_slug(&entries[target_idx], &base) {
                    Some(owner) => format!("already the group child for slug '{owner}'"),
                    None => {
                        if text_at(&entries[target_idx], "parent") == Some(epic_id.as_str()) {
                            format!(
                                "already this epic's group child on a legacy plan path \
                                 ({}) that no longer matches the epic doc",
                                text_at(&entries[target_idx], "plan_path").unwrap_or("")
                            )
                        } else {
                            format!(
                                "already a group child of another epic ({})",
                                text_at(&entries[target_idx], "plan_path").unwrap_or("")
                            )
                        }
                    }
                };
                return Err(DecomposeErr::new(
                    format!(
                        "group '{}' adopts {target_id}, which is {whose}; demoting a \
                         group into a task reshapes the epic",
                        grp.slug
                    ),
                    2,
                ));
            }
            let outcome = contain_into(
                entries,
                &owner_id,
                target_idx,
                &format!("group '{}'", grp.slug),
            )?;
            if let Some(reason) = outcome.warning {
                uncontained.push(format!(
                    "warning: adopted {target_id} into group '{}' but did NOT mark it \
                     contained: it {reason}, so it is its own delivery unit. It stays \
                     separately dispatchable, separately costed, and is not closed by \
                     the group's merge.",
                    grp.slug
                ));
            }
            if outcome.adopted {
                adopted.push(target_id);
            }
        }
        results.push(serde_json::json!({
            "id": owner_id,
            "slug": grp.slug,
            "waves": grp.waves,
            "action": action,
            "adopted": adopted,
        }));
    }

    // Pass 2: titles + inter-group blocked_by now that all ids exist, and
    // the dep tier classified against the doc's pin. Contract fields are
    // stamped ONLY on a contract dep and popped on hard, so a re-decompose
    // downgrade cleans stale stub metadata.
    let mut downgrades: Vec<String> = Vec::new();
    for (node_idx, gi) in &plan_to_group {
        let grp = &norm[*gi];
        let row = &mut entries[*node_idx];
        if let Some(obj) = row.as_object_mut() {
            obj.insert("title".into(), Value::String(grp.title.clone()));
            if grp.waves.is_empty() {
                obj.insert("details".into(), Value::Null);
            } else {
                obj.insert(
                    "details".into(),
                    Value::String(format!("Waves {} of epic {epic_id}", grp.waves)),
                );
            }
            let blocked_by: Vec<Value> = grp
                .blocked_by_groups
                .iter()
                .filter_map(|d| slug_to_id.get(d).cloned())
                .map(Value::String)
                .collect();
            obj.insert("blocked_by".into(), Value::Array(blocked_by.clone()));
        }
        let (dep, stub_against, cversion, downgrade) =
            classify_group_dep(grp, &pinned_versions, &base);
        if let Some(d) = downgrade {
            downgrades.push(d);
        }
        let r = results
            .iter_mut()
            .find(|r| r.get("slug").and_then(Value::as_str) == Some(grp.slug.as_str()))
            .expect("receipt row exists for every group");
        if let Some(obj) = r.as_object_mut() {
            if dep == "contract" {
                let node_obj = entries[*node_idx].as_object_mut().expect("row");
                node_obj.insert("dep".into(), Value::String("contract".into()));
                if let Some(sa) = &stub_against {
                    node_obj.insert("stub_against".into(), Value::String(sa.clone()));
                }
                if let Some(cv) = cversion {
                    node_obj.insert("contract_version".into(), Value::Number(cv.into()));
                }
                obj.insert("dep".into(), Value::String("contract".into()));
            } else {
                let node_obj = entries[*node_idx].as_object_mut().expect("row");
                node_obj.remove("dep");
                node_obj.remove("stub_against");
                node_obj.remove("contract_version");
                obj.insert("dep".into(), Value::String("hard".into()));
            }
        }
    }

    // Surface any unshipped orphans (slug dropped from the spec). They are
    // left in place, not deleted - deleting graph nodes is destructive.
    let orphan_ids: Vec<String> = orphans
        .iter()
        .filter_map(|o| text_at(o, "id"))
        .map(String::from)
        .collect();
    let mut unadopted_ids: Vec<String> = Vec::new();
    let mut contained_unadopted: Vec<String> = Vec::new();
    for e in entries.iter() {
        let Some(id) = text_at(e, "id") else {
            continue;
        };
        if text_at(e, "parent") == Some(epic_id.as_str()) && !is_group_child(e) {
            unadopted_ids.push(id.to_string());
            if text_at(e, "contained_in") == Some(epic_id.as_str()) {
                contained_unadopted.push(id.to_string());
            }
        }
    }
    Ok(MutateOut {
        epic_id,
        base,
        base_abs,
        epic_cwd,
        results,
        orphan_ids,
        unadopted_ids,
        contained_unadopted,
        uncontained,
        downgrades,
    })
}

/// One fan-out decision row.
struct FanoutRow {
    id: String,
    decision: String,
    reason: String,
    lane: &'static str,
    owned: bool,
}

fn decompose_post_steps(
    graph: &Path,
    out: MutateOut,
    norm: Vec<NormalizedGroup>,
    json_mode: bool,
) -> i32 {
    // Shared post-mutation re-read: the scaffold loop reads each child's
    // created_at + plan_path from it, and the fan-out reuses it. A read
    // failure degrades to an empty map rather than wedging the committed
    // mutation.
    let by_id: std::collections::BTreeMap<String, Value> =
        match crate::graph_store::read_rows(graph) {
            Ok(rows) => rows
                .iter()
                .filter_map(|e| text_at(e, "id").map(|i| (i.to_string(), e.clone())))
                .collect(),
            Err(_) => Default::default(),
        };
    for uc in out
        .uncontained
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
    {
        eprintln!("{uc}");
    }
    // The epic's why digest + its own strategy, read from the SAME doc text.
    let mut why_digest = String::new();
    let mut epic_strategy: Option<Value> = None;
    if let Some(base_abs) = &out.base_abs {
        if let Ok(epic_text) = std::fs::read_to_string(base_abs) {
            let (digest, warn) = extract_why_digest(&epic_text);
            if let Some(w) = warn {
                eprintln!("warning: {w}");
            }
            why_digest = digest;
            epic_strategy = epic_strategy_from_doc(&epic_text);
        }
    }
    // Scaffold loop: three skips (already linked, legacy file exists,
    // canonical already on disk), then write the self-contained stub.
    let mut scaffolded: Vec<String> = Vec::new();
    let source_doc = out.base.clone();
    let id_by_slug: std::collections::BTreeMap<&str, &str> = out
        .results
        .iter()
        .filter_map(|r| Some((r.get("slug")?.as_str()?, r.get("id")?.as_str()?)))
        .collect();
    for grp in &norm {
        let Some(child_id) = id_by_slug.get(grp.slug.as_str()) else {
            continue;
        };
        let child = by_id.get(*child_id);
        if child
            .map(|c| {
                text_at(c, "plan_path")
                    .map(|p| !p.is_empty())
                    .unwrap_or(false)
            })
            .unwrap_or(false)
        {
            continue; // never spawn a stub beside a filled plan
        }
        if Path::new(&separate_plan_path(
            &out.base_abs.as_deref().unwrap_or(&out.base),
            &grp.slug,
        ))
        .exists()
        {
            continue; // grandfather a legacy .group-<slug>.md file in place
        }
        let child_root = child
            .and_then(|c| text_at(c, "cwd"))
            .map(String::from)
            .or_else(|| out.epic_cwd.clone())
            .unwrap_or_else(|| {
                super::create_cli::repo_root(
                    &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                )
            });
        let canonical = canonical_child_plan_path(
            &grp.slug,
            child_id,
            &child_root,
            child.and_then(|c| text_at(c, "created_at")),
        );
        if canonical.is_empty() || Path::new(&canonical).exists() {
            continue; // idempotent re-run
        }
        let adopted_nodes: Vec<(String, String)> = grp
            .adopt
            .iter()
            .map(|aid| {
                (
                    aid.clone(),
                    by_id
                        .get(aid)
                        .and_then(|n| text_at(n, "title"))
                        .unwrap_or(aid)
                        .to_string(),
                )
            })
            .collect();
        if let Some(dir) = Path::new(&canonical).parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("warning: could not scaffold separate plan {canonical}: {e}");
                continue;
            }
        }
        let body = scaffold_separate_plan(
            grp,
            &out.epic_id,
            &source_doc,
            &why_digest,
            &adopted_nodes,
            None,
            epic_strategy.as_ref(),
        );
        match std::fs::write(&canonical, body) {
            Ok(_) => scaffolded.push(canonical),
            Err(e) => {
                // Non-fatal: the graph is already the source of truth. Warn
                // loudly so the missing stub is visible, never silently
                // swallowed.
                eprintln!("warning: could not scaffold separate plan {canonical}: {e}");
            }
        }
    }

    // 4a. Design fan-out: children flagged needs_think (or in the opt-in
    // wave-0 lane) that still need a design pass get a consented /think ->
    // /blueprint worker; the receipt names who owns them. Best-effort: a
    // failure names the children that stay the caller's to inline-fill.
    let flagged_slugs: std::collections::BTreeSet<&str> = norm
        .iter()
        .filter(|g| g.needs_think)
        .map(|g| g.slug.as_str())
        .collect();
    let mut fanout: Vec<FanoutRow> = Vec::new();
    let spec_ids: Vec<String> = out
        .results
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_str))
        .map(String::from)
        .collect();
    if !spec_ids.is_empty() {
        // wave-0 lane: opt-in via config; only genuinely independent
        // children (no intra-epic blocker) qualify.
        let mut wave0_ids: std::collections::BTreeSet<String> = Default::default();
        let wave0_env = std::env::var("FNO_THINK_SPAWN_WAVE0")
            .ok()
            .map(|v| ["1", "true", "yes", "on"].contains(&v.trim().to_lowercase().as_str()));
        let wave0_armed = wave0_env.unwrap_or_else(|| {
            crate::agents_config::config_lookup(
                &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                &["think_spawn", "on_decompose_wave0"],
            )
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        });
        if wave0_armed {
            if let Ok(rows) = crate::graph_store::read_rows(graph) {
                let (wave_by_id, _max) =
                    crate::plan_doc::rollup::compute_waves(&out.epic_id, &rows);
                for (cid, w) in wave_by_id {
                    if w == 0 {
                        wave0_ids.insert(cid);
                    }
                }
            }
        }
        let slug_by_id: std::collections::BTreeMap<&str, &str> = out
            .results
            .iter()
            .filter_map(|r| Some((r.get("id")?.as_str()?, r.get("slug")?.as_str()?)))
            .collect();
        for cid in &spec_ids {
            let Some(child) = by_id.get(cid) else {
                continue;
            };
            let rung = crate::backlog_ready::plan_rung(child);
            if rung != "none" && rung != "idea" {
                continue; // already designed; nothing for a /think to add
            }
            let lane = if flagged_slugs
                .contains(slug_by_id.get(cid.as_str()).copied().unwrap_or(""))
                || wave0_ids.contains(cid)
            {
                if wave0_ids.contains(cid)
                    && !flagged_slugs.contains(slug_by_id.get(cid.as_str()).copied().unwrap_or(""))
                {
                    "wave0"
                } else {
                    "needs_think"
                }
            } else {
                continue;
            };
            let row = fanout_spawn_think(child, &why_digest, lane);
            fanout.push(row);
        }
    }

    // 4b. Report what happened.
    if json_mode {
        println!(
            "{}",
            serde_json::json!({
                "epic": out.epic_id,
                "groups": out.results,
                "orphaned": out.orphan_ids,
                "downgrades": out.downgrades,
                "packaging": "separate",
                "scaffolded": scaffolded,
                "fanout": fanout.iter().map(|f| serde_json::json!({
                    "id": f.id, "decision": f.decision, "reason": f.reason,
                    "lane": f.lane, "owned": f.owned,
                })).collect::<Vec<_>>(),
            })
        );
    } else {
        println!("epic: {}", out.epic_id);
        println!(
            "decomposed into {} group child node(s) (packaging: separate):",
            out.results.len()
        );
        for r in &out.results {
            let waves = match r.get("waves").and_then(Value::as_str) {
                Some(w) if !w.is_empty() => format!(" waves {w}"),
                _ => String::new(),
            };
            let blk = match r.get("blocked_by").and_then(Value::as_array) {
                Some(b) if !b.is_empty() => {
                    format!(
                        " blocked_by={}",
                        serde_json::to_string(b).unwrap_or_default()
                    )
                }
                _ => String::new(),
            };
            let tier = if r.get("dep").and_then(Value::as_str) == Some("contract") {
                " dep=contract"
            } else {
                ""
            };
            let adopted = match r.get("adopted").and_then(Value::as_array) {
                Some(a) if !a.is_empty() => {
                    format!(" adopted={}", serde_json::to_string(a).unwrap_or_default())
                }
                _ => String::new(),
            };
            println!(
                "  {}: {} ({}){waves}{blk}{tier}{adopted}",
                r.get("action").and_then(Value::as_str).unwrap_or("?"),
                r.get("id").and_then(Value::as_str).unwrap_or("?"),
                r.get("slug").and_then(Value::as_str).unwrap_or("?"),
            );
        }
        for f in &scaffolded {
            println!("  scaffolded plan: {f}");
        }
        let owned: Vec<&str> = fanout
            .iter()
            .filter(|f| f.owned)
            .map(|f| f.id.as_str())
            .collect();
        for f in &fanout {
            if f.decision == "spawned" {
                println!("  fan-out design pass dispatched: {}", f.id);
            }
        }
        if !owned.is_empty() {
            println!("  fan-out OWNS (do NOT inline-fill): {}", owned.join(", "));
        }
        let unowned: Vec<&str> = fanout
            .iter()
            .filter(|f| !f.owned)
            .map(|f| f.id.as_str())
            .collect();
        if !unowned.is_empty() {
            println!(
                "  fan-out did NOT claim (inline-fill these): {}",
                unowned.join(", ")
            );
        }
        if !out.orphan_ids.is_empty() {
            eprintln!(
                "warning: {} group child node(s) no longer in the spec, left in place: {}",
                out.orphan_ids.len(),
                out.orphan_ids.join(", ")
            );
        }
        for msg in &out.downgrades {
            eprintln!("warning: {msg}");
        }
    }

    // 4c. Name the epic children no group adopted (US3), on BOTH report
    // paths; stderr never pollutes the JSON on stdout.
    if !out.unadopted_ids.is_empty() {
        let n = out.contained_unadopted.len();
        let clause = if n > 0 {
            format!(
                "; {n} of them are contained in the epic itself, which has no PR, \
                 so add them to a group's adopt list or they never close"
            )
        } else {
            String::new()
        };
        eprintln!(
            "warning: {} epic child(ren) adopted by no group, left parented to the \
             epic: {}. Add them to a group's `adopt` list to package them into that \
             PR{}.",
            out.unadopted_ids.len(),
            out.unadopted_ids.join(", "),
            clause
        );
    }

    // The shared doc graduates after the FIRST group ships unless the
    // expected child count is recorded; the writer is native
    // (plan_doc::stamp::cmd_set_expected), best-effort, non-fatal.
    if let Some(base_abs) = &out.base_abs {
        if out.results.len() >= 1 {
            let res = crate::plan_doc::stamp::cmd_set_expected(
                Path::new(base_abs),
                out.results.len() as u32,
                false,
            );
            if res.exit != 0 {
                eprintln!(
                    "warning: could not record expected_url_count={} on {}: {}. The \
                     shared doc will graduate after the FIRST group ships unless you \
                     run: fno do plan set-expected --plan-path {} --count {}",
                    out.results.len(),
                    out.base,
                    res.message,
                    out.base,
                    out.results.len()
                );
            }
        }
    }

    // Mirror nav fields + forward status onto the epic and every created
    // child's plan (best-effort, never wedges the mutation).
    let mut project_ids: Vec<String> = vec![out.epic_id.clone()];
    for r in &out.results {
        if r.get("action").and_then(Value::as_str) == Some("created") {
            if let Some(id) = r.get("id").and_then(Value::as_str) {
                project_ids.push(id.to_string());
            }
        }
    }
    if let Ok(fresh) = crate::graph_store::read_rows(graph) {
        crate::plan_doc::project::project_graph_nodes(&fresh, &project_ids, None, None, None, None);
    }
    0
}

/// One consented fan-out dispatch: a fire-and-forget /think worker carrying
/// the seed prompt (origin pointer + transcribed why + the /blueprint chain
/// instruction), named per the dispatcher's provenance naming. The wheel
/// resolved harness/model through resolve_dispatch/node_model before
/// spawning; the port leaves both to the spawn verb's own defaults (that
/// resolution ladder is not decompose-owned).
fn fanout_spawn_think(child: &Value, why_digest: &str, lane: &'static str) -> FanoutRow {
    let node_id = text_at(child, "id").unwrap_or("?").to_string();
    let slug = text_at(child, "slug").unwrap_or("");
    let title = text_at(child, "title").unwrap_or("");
    let details = text_at(child, "details").unwrap_or("");
    let harness = text_at(child, "source_harness").unwrap_or("?");
    let session = text_at(child, "source_session_id").unwrap_or("?");
    let origin_cwd = text_at(child, "source_cwd").unwrap_or("?");

    // Resolve the pointer, never paraphrase (US1). The seed references the
    // transcript when resolvable, the stored triple when not.
    let (transcript, _resolved) = super::birth::resolve_transcript(harness, session, origin_cwd);
    let why_lines = match &transcript {
        Some(p) => vec![format!("  origin transcript: {}", p.display())],
        None => vec![format!(
            "  origin transcript UNRESOLVED (reason: foreign harness or pruned \
             file); fall back to the stored pointer:\n  origin session: {harness}:{session} @ {origin_cwd}"
        )],
    };
    let mut prompt = format!(
        "/think {node_id}\n\nWHY THIS NODE EXISTS - read the originating context \
         below for the full reasoning that justified filing it. Do NOT work from \
         the title alone; the transcript holds the discovery / failure-mode / \
         tradeoff that made this worth filing.\n{}\n",
        why_lines.join("\n")
    );
    if !why_digest.trim().is_empty() {
        prompt.push_str(&format!(
            "\nGROUNDING (transcribed from the epic - use it even if the transcript \
             above is unresolved):\n{}\n",
            why_digest.trim()
        ));
    }
    prompt.push_str(&format!(
        "\nNode: {node_id}{}\nTitle: {title}\n{}\nTHEN, in this SAME session, \
         CONTINUE into /blueprint - do not stop at /think. Run:\n  /blueprint \
         <your /think doc>\nwhich turns the design into an execution plan and \
         links it to node {node_id} (that link is what flips this child ready \
         for dispatch). A design pass that ends without a linked plan_path is a \
         FAILED pass; if /blueprint did not link it, run `fno backlog update \
         {node_id} plan_path=<doc>` yourself.\n",
        if slug.is_empty() {
            String::new()
        } else {
            format!(" ({slug})")
        },
        if details.is_empty() {
            String::new()
        } else {
            format!("\nDetails:\n{details}\n")
        },
    ));

    let slug_opt = if slug.is_empty() { None } else { Some(slug) };
    let name =
        crate::naming::dispatch_agent_name(Some("th"), "th", &node_id, slug_opt, None, None, None);
    let name = match name {
        Ok(n) => n,
        Err(e) => {
            return FanoutRow {
                id: node_id,
                decision: "skipped".into(),
                reason: format!("name mint refused: {e}"),
                lane,
                owned: false,
            };
        }
    };
    let node_cwd = text_at(child, "cwd").filter(|c| !c.trim().is_empty());
    let mut cmd = std::process::Command::new(crate::scrape::fno_bin());
    cmd.args(["agents", "spawn"]);
    match node_cwd {
        Some(c) => {
            cmd.args(["--cwd", c]);
        }
        None => {
            cmd.arg("--fresh");
        }
    }
    cmd.args(["--name", &name, "--", &prompt]);
    cmd.env("FNO_SPAWN_TRIGGER", format!("think_spawn:birth"));
    let out = cmd.output();
    match out {
        Ok(o) if o.status.success() => FanoutRow {
            id: node_id,
            decision: "spawned".into(),
            reason: String::new(),
            lane,
            owned: true,
        },
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            FanoutRow {
                id: node_id,
                decision: "skipped".into(),
                reason: if stderr.is_empty() {
                    format!("fno agents spawn exited {}", o.status.code().unwrap_or(-1))
                } else {
                    stderr
                },
                lane,
                owned: false,
            }
        }
        Err(e) => FanoutRow {
            id: node_id,
            decision: "skipped".into(),
            reason: format!("could not run fno agents spawn: {e}"),
            lane,
            owned: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grp(slug: &str, deps: &[&str]) -> NormalizedGroup {
        NormalizedGroup {
            slug: slug.to_string(),
            title: format!("Group {slug}"),
            waves: String::new(),
            blocked_by_groups: deps.iter().map(|s| s.to_string()).collect(),
            project: None,
            cwd: None,
            dep: "hard".to_string(),
            stub_against: None,
            needs_think: false,
            adopt: Vec::new(),
        }
    }

    #[test]
    fn wave_numbers_parses_ranges_lists_and_garbage() {
        assert_eq!(wave_numbers("1-2,4"), vec![1, 2, 4]);
        assert_eq!(wave_numbers("4"), vec![4]);
        assert_eq!(wave_numbers("1,3"), vec![1, 3]);
        assert_eq!(wave_numbers(" 2 - 3 "), vec![2, 3]);
        assert!(wave_numbers("").is_empty());
        assert!(wave_numbers("zero").is_empty());
        assert!(wave_numbers("5-2").is_empty());
    }

    #[test]
    fn slugs_reject_bad_shapes() {
        assert!(group_slug_ok("auth-refactor"));
        assert!(group_slug_ok("A1"));
        assert!(!group_slug_ok("-lead"));
        assert!(!group_slug_ok("has space"));
        assert!(!group_slug_ok(""));
    }

    #[test]
    fn contract_versions_read_both_pin_shapes() {
        let doc = "## Interface Contract\n\n**contract_version: 2**\n\n### Contract v3\n\ntext\n\n## Next\n";
        let v = extract_contract_versions(doc);
        assert!(v.contains(&2) && v.contains(&3));
        assert!(extract_contract_versions("no pin here").is_empty());
        // A pin outside the section body does not count.
        assert!(extract_contract_versions(
            "**contract_version: 9**\n\n## Interface Contract\n\nnone\n"
        )
        .is_empty());
    }

    #[test]
    fn contract_without_a_pin_downgrades_and_names_why() {
        let mut g = grp("b", &["a"]);
        g.dep = "contract".to_string();
        let (dep, stub, ver, why) = classify_group_dep(&g, &Default::default(), "doc.md");
        assert_eq!(dep, "hard");
        assert!(stub.is_none() && ver.is_none());
        assert!(why.unwrap().contains("pins no ## Interface Contract"));
        let mut pinned = std::collections::BTreeSet::new();
        pinned.insert(7u32);
        let (dep, stub, ver, why) = classify_group_dep(&g, &pinned, "doc.md");
        assert_eq!(dep, "contract");
        assert_eq!(stub.as_deref(), Some("doc.md#interface-contract"));
        assert_eq!(ver, Some(7));
        assert!(why.is_none());
    }

    #[test]
    fn child_identity_reads_field_then_legacy_paths() {
        let base = "plans/epic.md";
        let mut n = serde_json::json!({"group_slug": "auth"});
        assert_eq!(group_child_slug(&n, base).as_deref(), Some("auth"));
        n = serde_json::json!({"plan_path": "plans/epic.md#group-pay"});
        assert_eq!(group_child_slug(&n, base).as_deref(), Some("pay"));
        n = serde_json::json!({"plan_path": "plans/epic.group-pay.md"});
        assert_eq!(group_child_slug(&n, base).as_deref(), Some("pay"));
        n = serde_json::json!({"plan_path": "plans/other.md#group-pay"});
        assert_eq!(group_child_slug(&n, base), None);
        // A legacy child of ANOTHER epic is still a group child unscoped.
        assert!(is_group_child(&n));
        assert!(!is_group_child(
            &serde_json::json!({"plan_path": "plans/plain.md"})
        ));
    }

    #[test]
    fn shipped_reads_any_completion_signal() {
        assert!(is_shipped(&serde_json::json!({"pr_number": 5})));
        assert!(is_shipped(
            &serde_json::json!({"completed_at": "2026-01-01"})
        ));
        assert!(!is_shipped(&serde_json::json!({"pr_number": null})));
        assert!(!is_shipped(&serde_json::json!({})));
    }

    #[test]
    fn cap_resolution_flag_tightens_frontmatter_and_invalid_fails_closed() {
        let (cap, src) =
            resolve_effective_cap(&MaxChildren::Value(6), Some(3), Some(4), Some("d.md"))
                .expect("tighten ok");
        assert_eq!(cap, 3);
        assert!(src.contains("tightening"));
        let (cap, _) = resolve_effective_cap(&MaxChildren::Value(6), None, Some(4), None)
            .expect("frontmatter wins");
        assert_eq!(cap, 6);
        let (cap, _) =
            resolve_effective_cap(&MaxChildren::Unset, None, Some(4), None).expect("config");
        assert_eq!(cap, 4);
        assert_eq!(
            resolve_effective_cap(&MaxChildren::Value(0), None, None, Some("d.md"))
                .unwrap_err()
                .exit_code,
            1
        );
        assert_eq!(
            resolve_effective_cap(&MaxChildren::Invalid("null".into()), None, None, None)
                .unwrap_err()
                .exit_code,
            1
        );
        assert!(resolve_effective_cap(&MaxChildren::Unset, None, None, None).is_err());
    }

    #[test]
    fn validate_refuses_overflow_dup_undeclared_and_cycles() {
        let spec = serde_json::json!([
            {"slug": "a", "title": "A"},
            {"slug": "b", "title": "B"},
        ]);
        assert_eq!(
            validate_groups(&spec, Some(1), Some("--max-prs 1"), None)
                .unwrap_err()
                .exit_code,
            1
        );
        let dup = serde_json::json!([
            {"slug": "a", "title": "A"},
            {"slug": "a", "title": "A2"},
        ]);
        assert!(validate_groups(&dup, None, None, None).is_err());
        let undeclared = serde_json::json!([
            {"slug": "a", "title": "A", "blocked_by_groups": ["ghost"]},
        ]);
        assert!(validate_groups(&undeclared, None, None, None).is_err());
        let contract_no_blocker = serde_json::json!([
            {"slug": "a", "title": "A", "dep": "contract"},
        ]);
        assert!(validate_groups(&contract_no_blocker, None, None, None).is_err());
        let cycle = serde_json::json!([
            {"slug": "a", "title": "A", "blocked_by_groups": ["b"]},
            {"slug": "b", "title": "B", "blocked_by_groups": ["a"]},
        ]);
        assert_eq!(
            validate_groups(&cycle, None, None, None)
                .unwrap_err()
                .exit_code,
            2
        );
        let adopt_clash = serde_json::json!([
            {"slug": "a", "title": "A", "adopt": ["x-aaaa"]},
            {"slug": "b", "title": "B", "adopt": ["x-aaaa"]},
        ]);
        assert!(validate_groups(&adopt_clash, None, None, None).is_err());
        let ok = serde_json::json!([
            {"slug": "a", "title": "A"},
            {"slug": "b", "title": "B", "blocked_by_groups": ["a"]},
        ]);
        assert_eq!(validate_groups(&ok, None, None, None).unwrap().len(), 2);
    }

    #[test]
    fn scaffold_carries_the_created_stamp_and_consolidation_block() {
        let g = grp("1", &[]);
        let text = scaffold_separate_plan(
            &g,
            "ab-epic0001",
            "big.md",
            "",
            &[],
            Some("2026-08-20"),
            None,
        );
        assert!(text.contains("created: 2026-08-20"), "{text}");
        assert!(text.contains("consolidation:"));
        assert!(text.contains("<!-- Consolidation:"));
        assert!(text.contains("status: idea"));
        assert!(text.contains("<!-- Why (from epic):"));
    }

    #[test]
    fn slice_carries_waves_and_drops_out_of_slice_blockers() {
        let strategy = serde_json::json!({
            "execution_mode": "sequential",
            "waves": [
                {"wave": 1, "mode": "parallel", "difficulty": "low", "tasks": ["t1", "t2"]},
                {"wave": 2, "mode": "sequential", "difficulty": "high", "tasks": ["t3"]},
            ],
            "tasks": [
                {"id": "t1", "title": "One", "surface": ["a.py"], "verify": "v1",
                 "acceptance": ["ac1"], "blocked_by": [], "blocked_by_declared": false},
                {"id": "t2", "title": "Two", "surface": [], "verify": "",
                 "acceptance": [], "blocked_by": ["t3"], "blocked_by_declared": true},
                {"id": "t3", "title": "Three", "surface": [], "verify": "",
                 "acceptance": [], "blocked_by": [], "blocked_by_declared": false},
            ],
        });
        let (block, why) = epic_wave_slice(Some(&strategy), "1");
        assert!(why.is_none());
        assert!(block.contains("wave: 1"));
        assert!(!block.contains("wave: 2"));
        assert!(block.contains("'t2'"));
        // t3 ships in a sibling child; within this slice t2 is unblocked.
        assert!(block.contains("blocked_by: []"));
        // No wave in range names the reason instead of a block.
        let (empty, why) = epic_wave_slice(Some(&strategy), "9");
        assert!(empty.is_empty());
        assert!(why.unwrap().contains("no wave in range"));
    }

    #[test]
    fn strategy_reader_scopes_to_the_section_and_reports_malformed() {
        let doc = "## Examples\n\n```yaml\nnot: a: strategy\n```\n\n## Execution Strategy\n\n```yaml\nexecution_mode: sequential\nwaves:\n  - wave: 1\n    mode: parallel\n    tasks: ['t1']\ntasks:\n  - id: 't1'\n    title: One\n```\n";
        let s = epic_strategy_from_doc(doc).expect("section parses");
        assert_eq!(
            s.get("execution_mode").and_then(Value::as_str),
            Some("sequential")
        );
        assert!(epic_strategy_from_doc("## Execution Strategy\n\n```yaml\n: :\n```\n").is_none());
        assert!(epic_strategy_from_doc("no section").is_none());
    }

    #[test]
    fn dead_owner_refusal_covers_deferred_and_superseded() {
        let deferred = serde_json::json!({"id": "ab-1", "deferred_at": "2026-01-01"});
        assert_eq!(
            refuse_dead_owner(&deferred, "group 'x'").unwrap().exit_code,
            2
        );
        let superseded = serde_json::json!({"id": "ab-1", "superseded_by": "ab-2"});
        let err = refuse_dead_owner(&superseded, "group 'x'").unwrap();
        assert!(err.message.contains("unsupersede"));
        assert!(refuse_dead_owner(&serde_json::json!({"id": "ab-1"}), "g").is_none());
        assert!(refuse_dead_owner(
            &serde_json::json!({"id": "ab-1", "completed_at": "2026-01-01"}),
            "g"
        )
        .is_none());
    }
}
