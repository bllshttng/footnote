//! One installer for footnote's OpenCode surface. Writes the stop bridge, one
//! command file per shipped verb, one translated agent file per shipped
//! agent, and the skill trees into the directories OpenCode already scans in
//! its global config dir, so every project - not just this repository -
//! sees the `fno:` names the dispatch seeds render. A manifest of every
//! written path and its content hash makes upgrade and uninstall honest:
//! nothing outside the manifest is ever removed, and a file the user edited
//! is kept and named, never destroyed.
//!
//! Python's opencode arm (`cli/src/fno/setup/integration.py`) was ported
//! here; the setup wizard and `fno doctor` read this module's JSON receipts
//! through the fno-agents door.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::paths::{dirs_home, worktree_repo_root};

/// A catalog read above this bound reports unknown rather than a wrong
/// answer: the unfiltered config dump measured 5.7 MB on one machine because
/// agent prompts are inlined, and a reader that slurps without a bound is a
/// reader that will be killed.
const MAX_CATALOG_BYTES: usize = 32 * 1024 * 1024;

const MANIFEST_FILE_PREFIX: &str = "opencode-install-";

/// One manifest per config dir, keyed by a short hash of its canonical path:
/// two `OPENCODE_CONFIG_DIR` values on one machine must never share install
/// records, or uninstalling one config dir would orphan the other's files.
pub fn manifest_path(conf: &Path) -> PathBuf {
    let key = std::fs::canonicalize(conf).unwrap_or_else(|_| conf.to_path_buf());
    let hash = blake3::hash(key.display().to_string().as_bytes()).to_hex();
    crate::plugin_install::state_root().join(format!("{MANIFEST_FILE_PREFIX}{}.json", &hash[..12]))
}

/// OpenCode scans the config dir for commands (singular `command/`), agents
/// (`agent/`) and skills (`skills/`); `OPENCODE_CONFIG_DIR` moves it, which
/// is also the test and scratch-install seam.
pub fn config_dir() -> PathBuf {
    std::env::var_os("OPENCODE_CONFIG_DIR")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_home().join(".config/opencode"))
}

fn is_footnote_tree(root: &Path) -> bool {
    root.join(".claude-plugin").join("plugin.json").is_file()
        && root
            .join("cli/src/fno/setup/assets/opencode/footnote.js")
            .is_file()
}

/// The footnote tree to install from: the plugin root the session already
/// resolves (env hints, then the `~/.fno/plugin-root` pointer), then the
/// filtered stage, then the repository around `cwd`. Refuses with every
/// candidate named rather than half-installing.
fn resolve_source(cwd: &Path) -> Result<PathBuf, String> {
    let mut tried: Vec<String> = Vec::new();
    if let Some(root) = crate::provider::plugin_root() {
        if is_footnote_tree(&root) {
            return Ok(root);
        }
        tried.push(format!("{} (incomplete)", root.display()));
    }
    let stage = crate::plugin_install::state_root()
        .join("plugin-stage")
        .join("fno");
    if is_footnote_tree(&stage) {
        return Ok(stage);
    }
    tried.push(stage.display().to_string());
    let repo = worktree_repo_root(cwd);
    if is_footnote_tree(&repo) {
        return Ok(repo);
    }
    tried.push(repo.display().to_string());
    Err(format!(
        "opencode install: no footnote tree to install from (looked at {}); \
         run `fno config setup` first, or point FNO_REPO_ROOT at a footnote checkout",
        tried.join(", ")
    ))
}

fn plugin_version(root: &Path) -> String {
    std::fs::read_to_string(root.join(".claude-plugin/plugin.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|v| {
            v.get("version")
                .and_then(|x| x.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// The first `key: value` in a `---` frontmatter block, quotes stripped.
fn frontmatter_value(md: &str, key: &str) -> Option<String> {
    let rest = md.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    for line in rest[..end].lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        let mut value = v.trim().to_string();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = value[1..value.len() - 1].to_string();
        }
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

fn split_frontmatter(md: &str) -> (&str, &str) {
    match md
        .strip_prefix("---")
        .and_then(|rest| rest.find("\n---").map(|end| (rest, end)))
    {
        Some((rest, end)) => (&rest[..end], rest[end + 5..].trim_start_matches('\n')),
        None => ("", md),
    }
}

fn yaml_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The command file name a verb installs as - the same `fno:<verb>` string
/// `render_verb_seed` and `opencode_run_tail` emit, so the catalog matches
/// what a dispatch asks for.
pub fn command_file_name(verb: &str) -> String {
    format!("fno:{verb}.md")
}

fn command_stub(verb: &str, description: &str) -> Vec<u8> {
    format!(
        "---\ndescription: {}\n---\n\
         Load the footnote skill \"{verb}\" with the skill tool, then execute it \
         in this session with these arguments:\n\n$ARGUMENTS\n",
        yaml_quote(description)
    )
    .into_bytes()
}

/// The OpenCode agent file for one shipped `agents/*.md`: the same mapping
/// the plugin translator performs, as frontmatter OpenCode reads before any
/// plugin runs. Bare model names are dropped so the child falls back to
/// OpenCode's default; a `provider/model` string passes through.
/// Restrictions follow the translator's contract: `disallowedTools` carries
/// into OpenCode's disable-only `tools` record, and a `tools` allowlist
/// CANNOT be expressed there, so the agent is skipped rather than installed
/// unrestricted. `None` means skipped-with-reason (already named on stderr).
fn agent_file(stem: &str, md: &str) -> Option<Vec<u8>> {
    let (front, body) = split_frontmatter(md);
    let field = |key: &str| -> Option<String> {
        front
            .lines()
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim() == key))
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    if field("tools").map(|v| v.starts_with('[')).unwrap_or(false) {
        eprintln!(
            "opencode install: agent {stem} skipped: a tools allowlist cannot be expressed in OpenCode's agent vocabulary; convert it to disallowedTools"
        );
        return None;
    }
    let description = field("description").unwrap_or_else(|| stem.to_string());
    let mut text = format!(
        "---\ndescription: {}\nmode: subagent\n",
        yaml_quote(&description)
    );
    if let Some(model) = field("model").filter(|m| m.contains('/')) {
        text.push_str(&format!("model: {model}\n"));
    }
    if let Some(disallowed) = field("disallowedTools").filter(|v| v.starts_with('[')) {
        let names: Vec<String> = disallowed
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(|x| x.trim().trim_matches('"').trim_matches('\'').to_lowercase())
            .filter(|x| !x.is_empty())
            .collect();
        if !names.is_empty() {
            let record = names
                .iter()
                .map(|n| format!("{n}: false"))
                .collect::<Vec<_>>()
                .join(", ");
            text.push_str(&format!("tools: {{{record}}}\n"));
        }
    }
    text.push_str("---\n\n");
    text.push_str(body.trim_start());
    Some(text.into_bytes())
}

fn walk_files(dir: &Path, rel: &str, out: &mut BTreeMap<String, Vec<u8>>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("opencode install: {e}"))?;
    for entry in entries.flatten() {
        let child_rel = format!("{rel}/{}", entry.file_name().to_string_lossy());
        let path = entry.path();
        if path.is_dir() {
            walk_files(&path, &child_rel, out)?;
        } else {
            let bytes = std::fs::read(&path).map_err(|e| format!("opencode install: {e}"))?;
            out.insert(child_rel, bytes);
        }
    }
    Ok(())
}

/// Everything one install writes, as `config-dir-relative path -> bytes`,
/// in a deterministic order so identical trees produce identical manifests.
fn build_entries(root: &Path) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut entries = BTreeMap::new();
    let bridge = root.join("cli/src/fno/setup/assets/opencode/footnote.js");
    entries.insert(
        "plugins/footnote.js".to_string(),
        std::fs::read(&bridge)
            .map_err(|e| format!("opencode install: {}: {e}", bridge.display()))?,
    );
    let verbs = crate::provider::read_verbs_from(root);
    for verb in &verbs {
        let description = std::fs::read_to_string(root.join("commands").join(format!("{verb}.md")))
            .ok()
            .and_then(|md| frontmatter_value(&md, "description"))
            .or_else(|| {
                std::fs::read_to_string(root.join("skills").join(verb).join("SKILL.md"))
                    .ok()
                    .and_then(|md| frontmatter_value(&md, "description"))
            })
            .unwrap_or_else(|| format!("footnote {verb}"));
        entries.insert(
            format!("command/{}", command_file_name(verb)),
            command_stub(verb, &description),
        );
    }
    if let Ok(agents) = std::fs::read_dir(root.join("agents")) {
        for entry in agents.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let md = std::fs::read_to_string(&path)
                .map_err(|e| format!("opencode install: {}: {e}", path.display()))?;
            if let Some(bytes) = agent_file(stem, &md) {
                entries.insert(format!("agent/fno:{stem}.md"), bytes);
            }
        }
    }
    if let Ok(skills) = std::fs::read_dir(root.join("skills")) {
        for entry in skills.flatten() {
            let path = entry.path();
            if !path.join("SKILL.md").is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            walk_files(&path, &format!("skills/{name}"), &mut entries)?;
        }
    }
    Ok(entries)
}

#[derive(Serialize, Debug)]
pub struct InstallReceipt {
    pub action: &'static str,
    /// "installed", or "partial" when user files were kept and named.
    pub status: &'static str,
    pub version: String,
    pub config_dir: String,
    pub written: usize,
    /// Entries already present with identical content: left untouched so a
    /// second install changes no modification time.
    pub skipped: usize,
    /// Files now on disk that footnote did not write and did not overwrite.
    pub kept: Vec<String>,
    pub removed: usize,
    pub manifest: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Manifest {
    version: String,
    /// config-dir-relative path -> blake3 hex of the bytes footnote wrote.
    files: BTreeMap<String, String>,
}

fn read_manifest(conf: &Path) -> Option<Manifest> {
    let path = manifest_path(conf);
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_manifest(conf: &Path, manifest: &Manifest) -> Result<(), String> {
    let path = manifest_path(conf);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("opencode install: {e}"))?;
    }
    let text = serde_json::to_string_pretty(manifest).unwrap_or_default() + "\n";
    std::fs::write(&path, text).map_err(|e| format!("opencode install: {e}"))
}

pub fn install(cwd: &Path) -> Result<InstallReceipt, String> {
    let root = resolve_source(cwd)?;
    let version = plugin_version(&root);
    let entries = build_entries(&root)?;
    let conf = config_dir();
    let mut manifest: Manifest = read_manifest(&conf).unwrap_or_default();
    manifest.version = version.clone();
    let mut written = 0;
    let mut skipped = 0;
    let mut removed = 0;
    let mut kept: Vec<String> = Vec::new();
    for (rel, bytes) in &entries {
        let dest = conf.join(rel);
        let hash = blake3::hash(bytes).to_hex().to_string();
        if manifest.files.get(rel) == Some(&hash) && dest.is_file() {
            skipped += 1;
            continue;
        }
        if dest.exists() && !manifest.files.contains_key(rel) {
            // Existing bytes decide: equal content is adopted (recorded as
            // ours, no write), differing content is the user's - the
            // 42-directory hazard is kept and named, never overwritten.
            let existing = std::fs::read(&dest).map_err(|e| format!("opencode install: {e}"))?;
            if blake3::hash(&existing).to_hex().as_str() == hash {
                manifest.files.insert(rel.clone(), hash);
                skipped += 1;
                continue;
            }
            kept.push(rel.clone());
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("opencode install: {e}"))?;
        }
        std::fs::write(&dest, bytes).map_err(|e| format!("opencode install: {e}"))?;
        manifest.files.insert(rel.clone(), hash);
        written += 1;
    }
    // Upgrade: entries the new tree no longer ships come off disk.
    let lost: Vec<String> = manifest
        .files
        .keys()
        .filter(|rel| !entries.contains_key(*rel))
        .cloned()
        .collect();
    for rel in &lost {
        if std::fs::remove_file(conf.join(rel)).is_ok() {
            removed += 1;
        }
        manifest.files.remove(rel);
    }
    write_manifest(&conf, &manifest)?;
    Ok(InstallReceipt {
        action: "install",
        status: if kept.is_empty() {
            "installed"
        } else {
            "partial"
        },
        version,
        config_dir: conf.display().to_string(),
        written,
        skipped,
        kept,
        removed,
        manifest: manifest_path(&conf).display().to_string(),
    })
}

#[derive(Serialize, Debug)]
pub struct UninstallReceipt {
    pub action: &'static str,
    /// "uninstalled", or "partial" when edited files were kept and named.
    pub status: &'static str,
    pub config_dir: String,
    pub removed: usize,
    pub kept: Vec<String>,
}

pub fn uninstall() -> Result<UninstallReceipt, String> {
    let conf = config_dir();
    let manifest = read_manifest(&conf).ok_or_else(|| {
        format!(
            "opencode uninstall: no manifest at {}; footnote installed nothing \
             there, so nothing is removed",
            manifest_path(&conf).display()
        )
    })?;
    let mut removed = 0;
    let mut kept: Vec<String> = Vec::new();
    for (rel, hash) in &manifest.files {
        let dest = conf.join(rel);
        match std::fs::read(&dest) {
            // A file the user has since edited is theirs; kept and named.
            Ok(bytes) if blake3::hash(&bytes).to_hex().as_str() == *hash => {
                std::fs::remove_file(&dest).map_err(|e| format!("opencode uninstall: {e}"))?;
                removed += 1;
            }
            Ok(_) => kept.push(rel.clone()),
            // Already gone (or unreadable-but-absent): nothing left to remove.
            Err(_) => removed += 1,
        }
        prune_empty_parents(&dest, &conf);
    }
    // The manifest is removed last so an interrupted uninstall is resumable.
    std::fs::remove_file(manifest_path(&conf)).map_err(|e| format!("opencode uninstall: {e}"))?;
    Ok(UninstallReceipt {
        action: "uninstall",
        status: if kept.is_empty() {
            "uninstalled"
        } else {
            "partial"
        },
        config_dir: conf.display().to_string(),
        removed,
        kept,
    })
}

fn prune_empty_parents(start: &Path, stop: &Path) {
    let mut dir = start.parent();
    while let Some(path) = dir {
        if path == stop || !path.starts_with(stop) {
            return;
        }
        // remove_dir succeeds only on an empty dir, so a sibling the user
        // owns stops the walk by existing.
        if std::fs::remove_dir(path).is_err() {
            return;
        }
        dir = path.parent();
    }
}

/// The manifest's installed names, grouped the way the catalogs name them.
fn installed_names(manifest: &Manifest) -> (BTreeSet<String>, BTreeSet<String>, BTreeSet<String>) {
    let mut commands = BTreeSet::new();
    let mut agents = BTreeSet::new();
    let mut skills = BTreeSet::new();
    for rel in manifest.files.keys() {
        if let Some(stem) = rel
            .strip_prefix("command/")
            .and_then(|r| r.strip_suffix(".md"))
        {
            commands.insert(stem.to_string());
        } else if let Some(stem) = rel
            .strip_prefix("agent/")
            .and_then(|r| r.strip_suffix(".md"))
        {
            agents.insert(stem.to_string());
        } else if let Some(rest) = rel.strip_prefix("skills/") {
            if let Some(name) = rest.split('/').next() {
                skills.insert(name.to_string());
            }
        }
    }
    (commands, agents, skills)
}

struct LoadedCatalog {
    commands: Option<BTreeSet<String>>,
    agents: Option<BTreeSet<String>>,
    skills: Option<BTreeSet<String>>,
}

fn run_bounded(cmd: &str, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() || out.stdout.len() > MAX_CATALOG_BYTES {
        return None;
    }
    Some(out.stdout)
}

/// What OpenCode actually loads right now, read with `--pure` so a mirroring
/// plugin's catalog cannot masquerade as footnote's install. Names only.
fn read_loaded_catalog(conf: &Path) -> LoadedCatalog {
    let conf_str = conf.display().to_string();
    let commands = run_bounded("opencode", &["debug", "config", "--pure"]).and_then(|out| {
        let value: serde_json::Value = serde_json::from_slice(&out).ok()?;
        let names_for = |key: &str| -> BTreeSet<String> {
            value
                .get(key)
                .and_then(|v| v.as_object())
                .map(|map| {
                    map.keys()
                        .filter(|n| n.starts_with("fno:"))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        };
        Some((names_for("command"), names_for("agent")))
    });
    let skills = run_bounded("opencode", &["debug", "skill", "--pure"]).and_then(|out| {
        let value: serde_json::Value = serde_json::from_slice(&out).ok()?;
        let list = value.as_array()?;
        let mut names = BTreeSet::new();
        let singular = conf.join("skill");
        for entry in list {
            let location = entry.get("location").and_then(|l| l.as_str()).unwrap_or("");
            let inside = location.starts_with(&conf_str)
                && (location.starts_with(&singular.display().to_string())
                    || location.starts_with(&conf.join("skills").display().to_string()));
            if inside {
                if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
                    names.insert(name.to_string());
                }
            }
        }
        Some(names)
    });
    match commands {
        Some((commands, agents)) => LoadedCatalog {
            commands: Some(commands),
            agents: Some(agents),
            skills,
        },
        None => LoadedCatalog {
            commands: None,
            agents: None,
            skills,
        },
    }
}

/// The version the install WOULD write now, when a footnote tree resolves.
/// A manifest whose version differs from this names a stale install.
fn source_version() -> Option<String> {
    let cwd = std::env::current_dir().unwrap_or_default();
    resolve_source(&cwd).ok().map(|root| plugin_version(&root))
}

/// Installed (manifest) versus loaded (`--pure` catalogs), with the
/// difference by name. A catalog read that fails or exceeds its bound
/// reports unknown rather than a wrong answer. A manifest whose version
/// differs from the resolvable source names a stale install.
pub fn status_json() -> serde_json::Value {
    let conf = config_dir();
    let manifest = read_manifest(&conf);
    let (cmds, agents, skills) = match &manifest {
        Some(m) => installed_names(m),
        None => Default::default(),
    };
    let loaded = read_loaded_catalog(&conf);
    let diff = |installed: &BTreeSet<String>, loaded: &Option<BTreeSet<String>>| {
        let missing: Vec<String> = loaded
            .as_ref()
            .map(|set| installed.difference(set).cloned().collect())
            .unwrap_or_default();
        let stale: Vec<String> = loaded
            .as_ref()
            .map(|set| set.difference(installed).cloned().collect())
            .unwrap_or_default();
        (missing, stale)
    };
    let (missing_commands, stale_commands) = diff(&cmds, &loaded.commands);
    let (missing_agents, stale_agents) = diff(&agents, &loaded.agents);
    let (missing_skills, stale_skills) = diff(&skills, &loaded.skills);
    let missing: Vec<String> = [missing_commands, missing_agents, missing_skills].concat();
    let stale: Vec<String> = [stale_commands, stale_agents, stale_skills].concat();
    let source = source_version();
    let behind = matches!((&manifest, &source), (Some(m), Some(sv)) if sv.as_str() != m.version);
    let status = match &manifest {
        None => "absent",
        Some(_) => {
            if !missing.is_empty() {
                "partial"
            } else if behind {
                "stale"
            } else {
                "installed"
            }
        }
    };
    json!({
        "action": "status",
        "status": status,
        "version": manifest.as_ref().map(|m| m.version.clone()),
        "source_version": source,
        "config_dir": conf.display().to_string(),
        "bridge_present": conf.join("plugins/footnote.js").is_file(),
        "manifest": manifest.as_ref().map(|m| json!({"path": manifest_path(&conf).display().to_string(), "files": m.files.len()})),
        "installed": {"commands": cmds, "agents": agents, "skills": skills},
        "loaded": {
            "commands": loaded.commands.as_ref().map(|s| serde_json::Value::Array(s.iter().cloned().map(serde_json::Value::String).collect())).unwrap_or_else(|| json!("unknown")),
            "agents": loaded.agents.as_ref().map(|s| serde_json::Value::Array(s.iter().cloned().map(serde_json::Value::String).collect())).unwrap_or_else(|| json!("unknown")),
            "skills": loaded.skills.as_ref().map(|s| serde_json::Value::Array(s.iter().cloned().map(serde_json::Value::String).collect())).unwrap_or_else(|| json!("unknown")),
        },
        "missing": missing,
        "stale_names": stale,
    })
}

/// The manifest-only verdict the setup adapter's is_installed needs: no
/// catalog read, so an adapter sweep never pays for two opencode spawns.
pub fn installed_status() -> serde_json::Value {
    let conf = config_dir();
    let source = source_version();
    match read_manifest(&conf) {
        None => json!({
            "action": "installed",
            "status": "absent",
            "config_dir": conf.display().to_string(),
            "bridge_present": conf.join("plugins/footnote.js").is_file(),
        }),
        Some(m) => {
            let complete = m.files.keys().all(|rel| conf.join(rel).is_file());
            let behind = source.as_deref().is_some_and(|sv| sv != m.version);
            let stale = complete && behind;
            json!({
                "action": "installed",
                "status": if !complete {
                    "partial"
                } else if stale {
                    "stale"
                } else {
                    "installed"
                },
                "config_dir": conf.display().to_string(),
                "version": m.version,
                "source_version": source,
                "bridge_present": conf.join("plugins/footnote.js").is_file(),
            })
        }
    }
}
