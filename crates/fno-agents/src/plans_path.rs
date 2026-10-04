//! The plans-path chain, ported from Python `fno.paths`
//! (`plans_content_dir` -> `plans_dir` -> `plan_doc_filename`): where plan
//! docs live for one project, and the filename a new doc takes. One owner:
//! the `fno do plan path` verb forwards here through the pydoor, and
//! `plans_dirs.rs` resolves every registered project in-process instead of
//! paying one Python CLI startup per project.
//!
//! Chain, highest tier first:
//!   1. `<root>/.claude/settings.local.json` -> `plansDirectory` (per-machine)
//!   2. `<root>/.claude/settings.json` -> `plansDirectory` (committed)
//!   3. `plans_dir` in config.toml: the `.fno/plans/` sentinel means
//!      `<space>/plans` (migrating a legacy checkout copy); a plain relative
//!      value anchors at the project root; anything with `~`, `$VAR` or
//!      `{vault}`/`{project}` templates resolves like Python `_resolve`.
//!   4. `plans_filename` renders the doc name: strftime codes plus
//!      `{slug}`/`{node}`, then the dash cleanup the Python leg applies.

use std::path::{Path, PathBuf};

use chrono::{Local, TimeZone, Utc};

use crate::agents_config::{config_lookup, within_search_ceiling};
use crate::paths::{canonical_repo_root, resolve_loose, space_slug, worktree_repo_root};

const DEFAULT_PLANS_DIR: &str = ".fno/plans/";
const DEFAULT_PLANS_FILENAME: &str = "%Y%m%d-{slug}-{node}.md";

/// The plans dir for `anchor` (the project root): chain tiers 1-2, then the
/// config tier. `None` when the chain errors; the probe treats that the way
/// it treated a failed verb: fewer accepted dirs, never a wrong one.
pub(crate) fn plans_content_dir(anchor: &Path) -> Option<PathBuf> {
    for name in ["settings.local.json", "settings.json"] {
        let path = anchor.join(".claude").join(name);
        // A discovered settings file outside the test config ceiling is not
        // read, matching the config tier (agents_config::within_search_ceiling).
        if !within_search_ceiling(&path) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(raw) = data.get("plansDirectory").and_then(|v| v.as_str()) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let p = PathBuf::from(raw);
        let joined = if p.is_absolute() { p } else { anchor.join(p) };
        return Some(resolve_loose(&joined));
    }
    plans_dir(anchor)
}

/// The config tier: `plans_dir` from config.toml. The `.fno/plans/` sentinel
/// (default or explicit) means `<space>/plans`, migrating a legacy
/// `<root>/.fno/plans` onto the space on the way.
fn plans_dir(anchor: &Path) -> Option<PathBuf> {
    let raw = config_lookup(anchor, &["plans_dir"])
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| DEFAULT_PLANS_DIR.to_string());
    if raw == DEFAULT_PLANS_DIR {
        let canonical = canonical_repo_root(anchor).unwrap_or_else(|| resolve_loose(anchor));
        let space = spaces_root(anchor, /*durable*/ false)
            .join(space_slug(&canonical))
            .join("plans");
        migrate_from_checkout(&anchor.join(".fno").join("plans"), &space);
        return Some(space);
    }
    let leading = raw.trim_start();
    let plain_relative = !leading.is_empty()
        && !leading.starts_with('/')
        && !leading.starts_with('~')
        && !raw.contains('$')
        && !raw.contains('{');
    if plain_relative {
        return Some(resolve_loose(&anchor.join(&raw)));
    }
    resolve_template(&raw, Some(anchor)).ok()
}

/// The save path for a NEW plan doc: resolved dir + rendered filename.
pub(crate) fn plan_doc_path(
    anchor: &Path,
    slug: &str,
    node: &str,
    at: Option<PinnedTimestamp>,
) -> Result<PathBuf, String> {
    let dir = plans_content_dir(anchor).ok_or_else(|| "plans dir unresolved".to_string())?;
    Ok(dir.join(plan_doc_filename(anchor, slug, node, at)?))
}

/// A pinned render time: epoch seconds, the wire form `--now` carries so a
/// recompute from a durable timestamp mints the same name on a later day.
/// Rendered in UTC: the durable stamp it recomputes (a node's `created_at`)
/// is a UTC ISO instant, so its date may not depend on the reader's timezone.
#[derive(Clone, Copy)]
pub struct PinnedTimestamp(i64);

impl PinnedTimestamp {
    pub fn from_epoch(secs: i64) -> Option<Self> {
        Utc.timestamp_opt(secs, 0)
            .single()
            .map(|_| PinnedTimestamp(secs))
    }

    fn utc_datetime(self) -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(self.0, 0).single().unwrap_or_else(|| {
            // from_epoch only constructs valid instants, so this never fires.
            Utc.timestamp_opt(0, 0).single().expect("epoch is valid")
        })
    }
}

/// The doc filename: `plans_filename` (strftime + `{slug}`/`{node}`), then
/// the Python leg's cleanup: collapse runs of `-`, degrade a dangling
/// `-.md`, strip leading dashes. A node id that the rendered name does not
/// carry is an error, not a silently id-less file.
pub(crate) fn plan_doc_filename(
    anchor: &Path,
    slug: &str,
    node: &str,
    at: Option<PinnedTimestamp>,
) -> Result<String, String> {
    let template = config_lookup(anchor, &["plans_filename"])
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| DEFAULT_PLANS_FILENAME.to_string());
    // Python strftime leaves a code it does not know (say %q) in the output
    // literally; chrono drops it. Wrap every code chrono cannot render in a
    // field-shaped literal so the rendered name carries it the same way.
    let template = preserve_unsupported_codes(&template);
    let items = chrono::format::strftime::StrftimeItems::new(&template);
    let stamped = match at {
        Some(at) => at.utc_datetime().format_with_items(items.clone()),
        // No pin names the wall clock: an interactive mint is dated today,
        // locally.
        None => Local::now().format_with_items(items),
    };
    let mut name = stamped.to_string();
    substitute_fields(&mut name, |field| match field {
        "slug" => Ok(Some(slug.to_string())),
        "node" => Ok(Some(node.to_string())),
        _ => Ok(None),
    })?;
    name = collapse_dashes(&name);
    if name.ends_with("-.md") {
        name.truncate(name.len() - "-.md".len());
        name.push_str(".md");
    }
    let trimmed = name.trim_start_matches('-').to_string();
    if !node.is_empty() && is_node_id(node) {
        let prefix = node.split('-').next().unwrap_or(node);
        if plan_filename_node_id(&trimmed, prefix).as_deref() != Some(node) {
            return Err(format!(
                "plan filename {trimmed:?} names {}, but requested node {node:?}",
                plan_filename_node_id(&trimmed, prefix).unwrap_or_else(|| "no node id".to_string())
            ));
        }
    }
    Ok(trimmed)
}

/// Python's strftime leaves a code it does not know literally in the name;
/// chrono renders some of those with its own meaning (`%q` is chrono's
/// quarter) or drops others. Rewriting an out-of-set `%X` to `%%X` makes
/// chrono emit `%` plus the bare letter, which is exactly the literal.
const PYTHON_STRFTIME_CODES: &[&str] = &[
    "a", "A", "b", "B", "c", "C", "d", "D", "e", "F", "g", "G", "h", "H", "I", "j", "m", "M", "n",
    "p", "r", "R", "S", "t", "T", "u", "U", "V", "w", "W", "x", "X", "y", "Y", "z", "Z",
];

/// Escape every `%X` code outside Python's strftime set to its literal form.
fn preserve_unsupported_codes(template: &str) -> String {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_alphabetic() {
            let code = &template[i + 1..i + 2];
            if PYTHON_STRFTIME_CODES.contains(&code) {
                out.push_str(&template[i..i + 2]);
            } else {
                out.push('%');
                out.push('%');
                out.push_str(&template[i + 1..i + 2]);
            }
            i += 2;
            continue;
        }
        let ch = template[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// The node id a plan filename encodes, if it ends with one and its prefix
/// matches. Mirrors Python `paths.plan_filename_node_id` (the lint and the
/// intake keep their Python copy; this port serves the renderer).
fn plan_filename_node_id(name: &str, prefix: &str) -> Option<String> {
    let base = name.split('#').next().unwrap_or(name);
    let dot = base.rfind('.')?;
    let stem = &base[..dot];
    let ext = &base[dot + 1..];
    if ext != "md" {
        return None;
    }
    // `-(prefix)-(hex)` at the stem's end; hex is 4-8 lowercase digits.
    let dash = stem.rfind('-')?;
    let hex = &stem[dash + 1..];
    if hex.len() < 4
        || hex.len() > 8
        || !hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return None;
    }
    let before = &stem[..dash];
    let pdash = before.rfind('-')?;
    let found = &before[pdash + 1..];
    if found.is_empty()
        || found.len() > 8
        || !found.starts_with(|c: char| c.is_ascii_lowercase())
        || !found.chars().skip(1).all(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    if found.trim_end_matches('-') != prefix.trim_end_matches('-') {
        return None;
    }
    Some(format!("{found}-{hex}"))
}

fn is_node_id(s: &str) -> bool {
    let Some((prefix, hex)) = s.split_once('-') else {
        return false;
    };
    let p = prefix.as_bytes();
    !p.is_empty()
        && p.len() <= 8
        && p[0].is_ascii_lowercase()
        && p[1..].iter().all(|b| b.is_ascii_alphanumeric())
        && (4..=8).contains(&hex.len())
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b.is_ascii_hexdigit() && b.is_ascii_lowercase()))
}

/// `{slug}`/`{node}` fields with `{{`/`}}` escapes, one pass, like Python
/// `str.format` on the validated template. The field closure answers
/// `Ok(Some(value))` for a known name, `Ok(None)` for an unknown one, or a
/// named error it wants propagated verbatim.
fn substitute_fields(
    text: &mut String,
    field: impl Fn(&str) -> Result<Option<String>, String>,
) -> Result<(), String> {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '{' if chars.get(i + 1) == Some(&'{') => {
                out.push('{');
                i += 2;
            }
            '{' => {
                let end = chars[i..].iter().position(|c| *c == '}').map(|p| i + p);
                let Some(end) = end else {
                    return Err("unterminated {{ in template".to_string());
                };
                let name: String = chars[i + 1..end].iter().collect();
                match field(&name) {
                    Ok(Some(value)) => out.push_str(&value),
                    Ok(None) => return Err(format!("unknown placeholder {{{name}}} in template")),
                    Err(e) => return Err(e),
                }
                i = end + 1;
            }
            '}' if chars.get(i + 1) == Some(&'}') => {
                out.push('}');
                i += 2;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    *text = out;
    Ok(())
}

/// Collapse every run of two or more dashes into one.
fn collapse_dashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    out
}

/// Python `_resolve`: expand `$VAR`, then `{vault}`/`{project}` templates
/// (unknown template errors), unescape `{{`/`}}`, expand `~`, anchor a
/// relative remainder at the project root, resolve loose.
pub(crate) fn resolve_template(raw: &str, project_root: Option<&Path>) -> Result<PathBuf, String> {
    let mut substituted = expandvars(raw);
    substitute_fields(&mut substituted, |field| match field {
        "vault" => vault_root(project_root.unwrap_or(Path::new("."))).map(Some),
        "project" => project_name(project_root).map(Some),
        _ => Ok(None),
    })
    .map_err(|e| format!("path template in {raw:?}: {e}"))?;
    let mut path = expanduser(&substituted);
    if !path.is_absolute() {
        if let Some(root) = project_root {
            path = root.join(path);
        }
    }
    Ok(resolve_loose(&path))
}

/// The Obsidian vault root: `obsidian.vault` (bare name -> `~/<name>`,
/// absolute or `~`-prefixed as-is) when `obsidian.enabled`.
pub(crate) fn vault_root(anchor: &Path) -> Result<String, String> {
    let enabled = config_lookup(anchor, &["obsidian", "enabled"])
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !enabled {
        return Err("path template uses {vault} but obsidian.enabled is false".to_string());
    }
    let vault = config_lookup(anchor, &["obsidian", "vault"])
        .and_then(|v| v.as_str().map(str::to_owned))
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "path template uses {vault} but obsidian.vault is not set".to_string())?;
    let expanded = expanduser(&vault);
    if expanded.is_absolute() {
        return Ok(expanded.to_string_lossy().into_owned());
    }
    let home =
        std::env::var("HOME").map_err(|_| "no HOME to anchor the vault under".to_string())?;
    Ok(Path::new(&home)
        .join(expanded)
        .to_string_lossy()
        .into_owned())
}

/// The stable project-folder name: `project.id` -> git-remote slug ->
/// checkout basename, rejecting anything that could escape its subtree.
pub(crate) fn project_name(project_root: Option<&Path>) -> Result<String, String> {
    let root = project_root.unwrap_or(Path::new("."));
    let pid = config_lookup(root, &["project", "id"])
        .and_then(|v| v.as_str().map(str::to_owned))
        .filter(|v| !v.is_empty());
    let name = match pid.clone() {
        Some(p) => p,
        None => git_remote_slug(root)
            .or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()))
            .ok_or_else(|| "project root has no name".to_string())?,
    };
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(format!("unsafe project name for internal/ path: {name:?}"));
    }
    if pid.is_none() {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            eprintln!(
                "fno: warning: config.project.id is unset; internal/ paths derive an \
unstable folder name ({name:?}) from the git remote or checkout dir. \
Set config.project.id to pin it and stop stray internal/<name>/ folders."
            );
        });
    }
    Ok(name)
}

/// Last path segment of `remote.origin.url`, one trailing `.git` stripped.
fn git_remote_slug(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            &root.to_string_lossy(),
            "config",
            "--get",
            "remote.origin.url",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let url = url.trim_end_matches('/');
    if url.is_empty() {
        return None;
    }
    let mut tail = url.rsplit(['/', ':']).next().unwrap_or("").to_string();
    if let Some(stripped) = tail.strip_suffix(".git") {
        tail = stripped.to_string();
    }
    if tail.is_empty() || tail.contains('/') || tail.contains('\\') {
        return None;
    }
    Some(tail)
}

/// POSIX `$VAR` / `${VAR}` expansion; unset reads empty, like
/// `os.path.expandvars`.
fn expandvars(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'{' {
                if let Some(end) = raw[i + 2..].find('}') {
                    out.push_str(&std::env::var(&raw[i + 2..i + 2 + end]).unwrap_or_default());
                    i += 2 + end + 1;
                    continue;
                }
            } else if bytes[i + 1].is_ascii_alphabetic() || bytes[i + 1] == b'_' {
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                out.push_str(&std::env::var(&raw[i + 1..j]).unwrap_or_default());
                i = j;
                continue;
            }
        }
        let ch = raw[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Leading `~`/`~/` expansion against `$HOME`.
pub(crate) fn expanduser(raw: &str) -> PathBuf {
    if raw == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

/// The spaces root, Python `paths.spaces_root`'s full chain:
/// `$FNO_SPACES_DIR` (skipped when `durable`, whose destination must survive
/// the pin) > `paths.spaces_dir` in config > `<state_dir>/spaces`, where
/// `state_dir` is `$FNO_STATE_DIR` > config `state_dir` > `~/.fno`. The
/// legacy Rust `space_dir` skips the config tiers; the plans chain keeps
/// them so a customized install's plans land where the Python leg put them.
fn spaces_root(anchor: &Path, durable: bool) -> PathBuf {
    if !durable {
        if let Some(v) = std::env::var_os("FNO_SPACES_DIR").filter(|v| !v.is_empty()) {
            let root = resolve_loose(&expanduser(&v.to_string_lossy()));
            crate::paths::fence_resolved_root(&root);
            return root;
        }
    }
    if let Some(override_) = config_lookup(anchor, &["paths", "spaces_dir"])
        .and_then(|v| v.as_str().map(str::to_owned))
        .filter(|v| !v.is_empty())
    {
        if let Ok(root) = resolve_template(&override_, Some(anchor)) {
            crate::paths::fence_resolved_root(&root);
            return root;
        }
    }
    let state = if let Some(v) = std::env::var_os("FNO_STATE_DIR").filter(|v| !v.is_empty()) {
        let root = resolve_loose(&expanduser(&v.to_string_lossy()));
        crate::paths::fence_resolved_root(&root);
        root
    } else {
        let raw = config_lookup(anchor, &["state_dir"])
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| "~/.fno/".to_string());
        match resolve_template(&raw, Some(anchor)) {
            Ok(root) => {
                crate::paths::fence_resolved_root(&root);
                root
            }
            Err(_) => std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".fno"))
                .unwrap_or_else(|| PathBuf::from(".fno")),
        }
    };
    state.join("spaces")
}

/// The nearest ancestor of `old` that is a checkout root, or None. Pure walk.
fn repo_root_of(old: &Path) -> Option<PathBuf> {
    let mut candidate = resolve_loose(old.parent()?);
    loop {
        if candidate.join(".git").exists() {
            return Some(candidate);
        }
        candidate = candidate.parent()?.to_path_buf();
    }
}

/// One-shot lazy migration onto the space, Python `migrate_from_checkout`:
/// move a legacy checkout dir to `new` unless either exists or old is a
/// symlink, then leave a MOVED-TO pointer. Best effort: any failure leaves
/// `old` in place and returns false. The destination must sit in the source
/// repository's DURABLE space (the env pin skipped), so a move never strands
/// the only copy behind a sandbox root.
fn migrate_from_checkout(old: &Path, new: &Path) -> bool {
    if old == new || new.exists() || !old.exists() {
        return false;
    }
    let is_symlink = std::fs::symlink_metadata(old)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true);
    if is_symlink {
        return false;
    }
    if let Some(repo) = repo_root_of(old) {
        let canonical = canonical_repo_root(&repo).unwrap_or_else(|| worktree_repo_root(&repo));
        let durable = spaces_root(&repo, /*durable*/ true).join(space_slug(&canonical));
        if !resolve_loose(new).starts_with(resolve_loose(&durable)) {
            return false;
        }
    }
    if let Some(parent) = new.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    if std::fs::rename(old, new).is_err() {
        if copy_dir_recursive(old, new).is_err() {
            return false;
        }
        if std::fs::remove_dir_all(old).is_err() {
            return false;
        }
    }
    let marker = old.parent().map(|p| p.join("MOVED-TO"));
    if let Some(marker) = marker {
        if !marker.exists() {
            if let Some(parent) = new.parent() {
                let _ = std::fs::write(&marker, format!("{}\n", parent.display()));
            }
        }
    }
    true
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// `fno-agents state plan-dir [cwd]`: the chain-resolved plans dir.
pub fn run_plan_dir(args: &[String]) -> i32 {
    let anchor = anchor_of(args);
    match plans_content_dir(&anchor) {
        Some(dir) => {
            println!("{}", dir.display());
            0
        }
        None => {
            eprintln!(
                "error: could not resolve the plans dir for {}",
                anchor.display()
            );
            1
        }
    }
}

/// `fno-agents state plan-path [--slug S] [--node N] [--name-only]
/// [--now EPOCH] [cwd]`: the save path (or bare filename) for a new plan
/// doc. The `fno do plan path` verb forwards here.
pub fn run_plan_path(args: &[String]) -> i32 {
    // An empty --slug is a real input the chain renders (an id-less,
    // slug-less date file); only a MISSING --slug is a usage error, which is
    // what the deleted Python verb's typer requirement did too.
    let mut slug: Option<String> = None;
    let mut node = String::new();
    let mut name_only = false;
    let mut now: Option<PinnedTimestamp> = None;
    let mut pos: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--slug" if i + 1 < args.len() => {
                i += 1;
                slug = Some(args[i].clone());
            }
            "--node" if i + 1 < args.len() => {
                i += 1;
                node = args[i].clone();
            }
            "--name-only" => name_only = true,
            "--now" if i + 1 < args.len() => {
                i += 1;
                match args[i]
                    .parse::<i64>()
                    .ok()
                    .and_then(PinnedTimestamp::from_epoch)
                {
                    Some(at) => now = Some(at),
                    None => {
                        eprintln!("error: --now wants epoch seconds, got {}", args[i]);
                        return 2;
                    }
                }
            }
            other if other.starts_with('-') => {
                eprintln!("error: unknown flag {other}");
                return 2;
            }
            _ => pos.push(args[i].clone()),
        }
        i += 1;
    }
    let Some(slug) = slug else {
        eprintln!("usage: fno-agents state plan-path --slug <slug> [--node <id>] [--name-only] [--now <epoch>] [cwd]");
        return 2;
    };
    let anchor = anchor_of(&pos);
    match plan_doc_path(&anchor, &slug, &node, now) {
        Ok(path) => {
            println!(
                "{}",
                if name_only {
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                } else {
                    path.to_string_lossy().into_owned()
                }
            );
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn anchor_of(args: &[String]) -> PathBuf {
    match args.first() {
        // An explicit root wins even when it does not exist yet: decompose
        // computes a child project's plans path before that repo is checked
        // out, and a silent cwd fallback answers for the wrong root.
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::test_env_lock;
    use std::fs;

    struct Fixture {
        base: PathBuf,
    }

    impl Fixture {
        /// One git repo at `base/proj`, empty config, isolated state roots.
        fn new(tag: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("fno-plans-path-{}-{}", tag, std::process::id()));
            let _ = fs::remove_dir_all(&base);
            let proj = base.join("proj");
            fs::create_dir_all(proj.join(".claude")).unwrap();
            fs::create_dir_all(proj.join(".fno")).unwrap();
            git_init(&proj);
            Fixture { base }
        }

        fn root(&self) -> PathBuf {
            self.base.join("proj")
        }

        fn config(&self) -> PathBuf {
            self.base.join("config.toml")
        }

        fn pins(&self) -> Vec<(&'static str, String)> {
            vec![
                ("FNO_CONFIG", self.config().display().to_string()),
                (
                    "FNO_STATE_DIR",
                    self.base.join("state").display().to_string(),
                ),
                (
                    "FNO_SPACES_DIR",
                    self.base.join("spaces").display().to_string(),
                ),
            ]
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    struct EnvGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn new(pins: &[(&'static str, String)]) -> Self {
            let saved = pins
                .iter()
                .map(|(k, _)| (*k, std::env::var_os(k)))
                .collect();
            for (k, v) in pins {
                std::env::set_var(k, v);
            }
            EnvGuard { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(old) => std::env::set_var(k, old),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    fn git_init(dir: &Path) {
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .args(["-C", dir.to_str().unwrap()])
                .args(args)
                .status()
                .unwrap()
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["commit", "-q", "--allow-empty", "-m", "init"]);
    }

    /// 2026-09-27 12:00:00 UTC. Every render pins this instant so the date
    /// codes in a template are deterministic (a pinned epoch renders in UTC,
    /// so `20260927` holds on every machine).
    const NOW: i64 = 1790505600;

    #[test]
    fn settings_local_json_plans_directory_wins() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("tier1");
        fs::write(
            fx.root().join(".claude/settings.local.json"),
            format!(
                r#"{{"plansDirectory": "{}"}}"#,
                fx.base.join("local-plans").display().to_string()
            ),
        )
        .unwrap();
        let _env = EnvGuard::new(&fx.pins());
        let dir = plans_content_dir(&fx.root()).unwrap();
        assert_eq!(dir, resolve_loose(&fx.base.join("local-plans")));
    }

    #[test]
    fn settings_json_plans_directory_is_the_second_tier() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("tier2");
        fs::write(
            fx.root().join(".claude/settings.json"),
            r#"{"plansDirectory": "docs/plans"}"#,
        )
        .unwrap();
        let _env = EnvGuard::new(&fx.pins());
        let dir = plans_content_dir(&fx.root()).unwrap();
        assert_eq!(dir, resolve_loose(&fx.root().join("docs/plans")));
    }

    #[test]
    fn settings_plans_directory_outside_the_ceiling_is_skipped() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("ceiling");
        fs::write(
            fx.root().join(".claude/settings.local.json"),
            r#"{"plansDirectory": "docs/plans"}"#,
        )
        .unwrap();
        fs::write(fx.config(), "plans_dir = \"cfg/plans\"\n").unwrap();
        let ceiling = fx.base.join("ceiling-root");
        fs::create_dir_all(&ceiling).unwrap();
        let _env = EnvGuard::new(&fx.pins());
        let _capped = EnvGuard::new(&[("FNO_CONFIG_SEARCH_ROOT", ceiling.display().to_string())]);

        let dir = plans_content_dir(&fx.root()).unwrap();
        assert_ne!(
            dir,
            resolve_loose(&fx.root().join("docs/plans")),
            "a settings file outside the ceiling is not read"
        );
        assert_eq!(
            dir,
            resolve_loose(&fx.root().join("cfg/plans")),
            "the explicit FNO_CONFIG tier still answers under the ceiling"
        );
    }

    #[test]
    fn config_plans_dir_plain_relative_anchors_at_root() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("tier3");
        let _env = EnvGuard::new(&fx.pins());
        fs::write(fx.config(), "plans_dir = \"notes/plans\"\n").unwrap();
        let dir = plans_content_dir(&fx.root()).unwrap();
        assert_eq!(dir, resolve_loose(&fx.root().join("notes/plans")));
    }

    #[test]
    fn config_plans_dir_default_sentinel_means_space_plans() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("tier3default");
        let _env = EnvGuard::new(&fx.pins());
        fs::write(fx.config(), "plans_dir = \".fno/plans/\"\n").unwrap();
        let dir = plans_content_dir(&fx.root()).unwrap();
        let slug = space_slug(&fs::canonicalize(&fx.root()).unwrap());
        let spaces = fs::canonicalize(&fx.base).unwrap().join("spaces");
        assert_eq!(
            dir,
            spaces.join(slug).join("plans"),
            "the sentinel resolves onto the project's space"
        );
    }

    #[test]
    fn default_plans_dir_keeps_an_unregistered_child_root() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("unregistered-child");
        let _env = EnvGuard::new(&fx.pins());
        let anchor = fx.base.join("unregistered/web");
        let node = ["x", "abcd"].join("-");
        let path = plan_doc_path(
            &anchor,
            "etl-search",
            &node,
            Some(PinnedTimestamp::from_epoch(NOW).unwrap()),
        )
        .unwrap();
        let expected = fx
            .base
            .join("spaces")
            .join(space_slug(&anchor))
            .join("plans");
        assert_eq!(path.parent(), Some(expected.as_path()));
    }

    #[test]
    fn config_plans_dir_default_sentinel_migrates_legacy_checkout_dir() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("tier3migrate");
        // The migration's durable-space guard refuses a destination behind
        // the per-process FNO_SPACES_DIR pin, so this test aims BOTH pins at
        // the same root: the chain then resolves through FNO_STATE_DIR and
        // the durable read agrees with the answer.
        let mut pins = fx.pins();
        let durable_spaces = fx.base.join("state").join("spaces");
        pins.retain(|(k, _)| *k != "FNO_SPACES_DIR");
        pins.push(("FNO_SPACES_DIR", durable_spaces.display().to_string()));
        let _env = EnvGuard::new(&pins);
        fs::write(fx.config(), "plans_dir = \".fno/plans/\"\n").unwrap();
        let legacy = fx.root().join(".fno/plans");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("20260101-old.md"), "zz").unwrap();
        let dir = plans_content_dir(&fx.root()).unwrap();
        let slug = space_slug(&fs::canonicalize(&fx.root()).unwrap());
        let spaces = fs::canonicalize(&fx.base)
            .unwrap()
            .join("state")
            .join("spaces");
        assert_eq!(dir, spaces.join(slug).join("plans"));
        assert!(dir.join("20260101-old.md").exists(), "legacy doc moved");
        assert!(!legacy.exists(), "legacy dir gone");
        assert!(
            legacy.parent().unwrap().join("MOVED-TO").exists(),
            "pointer left behind"
        );
    }

    #[test]
    fn config_plans_dir_vault_template_resolves() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("tier3vault");
        let mut pins = fx.pins();
        pins.push(("HOME", fx.base.display().to_string()));
        let _env = EnvGuard::new(&pins);
        fs::write(
            fx.config(),
            "plans_dir = \"{vault}/internal/fno/plans\"\n\n[obsidian]\nenabled = true\nvault = \"myvault\"\n",
        )
        .unwrap();
        let dir = plans_content_dir(&fx.root()).unwrap();
        assert_eq!(
            dir,
            resolve_loose(&fx.base.join("myvault/internal/fno/plans"))
        );
    }

    #[test]
    fn plans_filename_renders_template_with_cleanup() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("filename");
        let _env = EnvGuard::new(&fx.pins());
        fs::write(fx.config(), "").unwrap();
        let name = plan_doc_filename(
            &fx.root(),
            "my-slug",
            "zz-11aa",
            PinnedTimestamp::from_epoch(NOW),
        )
        .unwrap();
        assert_eq!(name, "20260927-my-slug-zz-11aa.md");

        // Cleanup: doubled dashes collapse, a dangling `-.md` degrades, and
        // leading dashes strip. An empty slug and node leave only the date.
        let name = plan_doc_filename(&fx.root(), "", "", PinnedTimestamp::from_epoch(NOW)).unwrap();
        assert_eq!(name, "20260927.md", "empty slug and node degrade cleanly");
    }

    #[test]
    fn plans_filename_keeps_codes_chrono_cannot_render() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("badcode");
        let _env = EnvGuard::new(&fx.pins());
        // Python strftime leaves %q literally in the output; the render must
        // too, not silently drop it.
        fs::write(fx.config(), "plans_filename = \"%q-%Y%m%d-{slug}.md\"\n").unwrap();
        let name =
            plan_doc_filename(&fx.root(), "feature", "", PinnedTimestamp::from_epoch(NOW)).unwrap();
        assert_eq!(name, "%q-20260927-feature.md");
    }

    #[test]
    fn plans_filename_node_mismatch_is_an_error() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fx = Fixture::new("nodemismatch");
        let _env = EnvGuard::new(&fx.pins());
        fs::write(fx.config(), "plans_filename = \"%Y%m%d-{slug}.md\"\n").unwrap();
        let err = plan_doc_filename(
            &fx.root(),
            "feature",
            "zz-11aa",
            PinnedTimestamp::from_epoch(NOW),
        )
        .unwrap_err();
        assert!(err.contains("zz-11aa"), "the refusal names the node: {err}");
    }

    #[test]
    fn filename_node_id_extraction_matches_python_shape() {
        assert_eq!(
            plan_filename_node_id("20260927-s-zz-11aa.md", "zz").as_deref(),
            Some("zz-11aa")
        );
        assert_eq!(plan_filename_node_id("20260927-s.md", "zz"), None);
        assert_eq!(
            plan_filename_node_id("20260927-s-y-1234.md", "zz"),
            None,
            "prefix mismatch contributes nothing"
        );
    }
}
