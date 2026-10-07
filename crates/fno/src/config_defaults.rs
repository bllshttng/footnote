//! `fno config get --defaults`: the defaults inventory.
//!
//! One row per key in `docs/config.example.toml` (embedded at build time):
//! the dotted key, its default, the effective value, and the source that
//! answered (`default`, `global`, `project`). Optional keys with no default
//! (the commented `<unset>` rows in the example) print `default: unset`.
//!
//! Claimed lexically beside `setup_autowire`: `config setup run` composes on
//! this inventory, writing only keys whose value differs from the default
//! (`differs`), so the answer must not depend on the Python wheel.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The generated config reference, every key at its default. Embedded so the
/// inventory cannot drift from the file the drift gate checks.
const EXAMPLE: &str = include_str!("../../../docs/config.example.toml");

/// One inventory row: the dotted key, its default rendered as text, the
/// effective value, and the source that answered.
#[derive(Debug)]
pub struct Row {
    pub key: String,
    pub default: String,
    pub value: String,
    pub source: &'static str,
}

/// Claim `config get --defaults [--json]` lexically, before clap, the way
/// `setup_autowire::classify` does: exact words, both flags in any order,
/// nothing else. Every other `config get` argv forwards to Python untouched.
pub fn classify(args: &[OsString]) -> Option<Vec<OsString>> {
    let words: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
    if words.len() != args.len() {
        return None;
    }
    let ["config", "get", rest @ ..] = words.as_slice() else {
        return None;
    };
    let mut defaults = false;
    for flag in rest {
        match *flag {
            "--defaults" => defaults = true,
            "--json" => {}
            _ => return None,
        }
    }
    if defaults {
        Some(args[2..].to_vec())
    } else {
        None
    }
}

/// Render a TOML value the way the wizard showed defaults: strings bare,
/// bools `true`/`false`, lists comma-joined.
fn render(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => d.to_string(),
        toml::Value::Array(a) => a.iter().map(render).collect::<Vec<_>>().join(","),
        toml::Value::Table(_) => String::new(),
    }
}

/// Flatten a parsed config into dotted keys. Nested tables join with `.`,
/// matching the key spellings every `fno config set` call uses.
fn flatten(table: &toml::Table, prefix: &str, out: &mut BTreeMap<String, toml::Value>) {
    for (k, v) in table {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            toml::Value::Table(t) => flatten(t, &key, out),
            other => {
                out.insert(key, other.clone());
            }
        }
    }
}

/// The example's defaults: every real key flattened, plus the optional keys
/// the example shows commented out with the `<unset>` marker. Returns the
/// map and the unset-key list, because `differs` treats an unset key as
/// answered by any concrete value.
pub fn example_inventory() -> (BTreeMap<String, toml::Value>, Vec<String>) {
    let parsed: toml::Table = toml::from_str(EXAMPLE).unwrap_or_default();
    let mut defaults = BTreeMap::new();
    flatten(&parsed, "", &mut defaults);
    // The optional rows are comments, so the TOML parser never sees them.
    // Walk the lines, tracking the current `[section]`, and pick up the
    // `# key = <unset>` rows the generator emits for no-default keys.
    let mut unset = Vec::new();
    let mut section = String::new();
    for line in EXAMPLE.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            section = t[1..t.len() - 1].to_string();
            continue;
        }
        let Some(rest) = t.strip_prefix("# ") else {
            continue;
        };
        let Some(eq) = rest.find(" = <unset>") else {
            continue;
        };
        let name = rest[..eq].trim();
        if !name.is_empty() && !name.contains(' ') {
            let key = if section.is_empty() {
                name.to_string()
            } else {
                format!("{section}.{name}")
            };
            unset.push(key);
        }
    }
    (defaults, unset)
}

/// Parse one config file into dotted keys. `Ok(None)` when the file is
/// absent; `Err` names the file, because a config that cannot be read must
/// stop the inventory rather than silently answer "default".
fn read_flat(path: &Path) -> Result<Option<BTreeMap<String, toml::Value>>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: cannot be read ({e})", path.display())),
    };
    let parsed: toml::Table =
        toml::from_str(&text).map_err(|e| format!("{}: not valid TOML ({e})", path.display()))?;
    let mut out = BTreeMap::new();
    flatten(&parsed, "", &mut out);
    Ok(Some(out))
}

/// The inventory itself: one row per example key, `project` beating `global`
/// beating `default`, exactly the precedence the Python loader applies.
pub fn inventory(global: Option<&Path>, project: Option<&Path>) -> Result<Vec<Row>, String> {
    let (defaults, unset) = example_inventory();
    let g = match global {
        Some(p) => read_flat(p)?,
        None => None,
    };
    let p = match project {
        Some(p) => read_flat(p)?,
        None => None,
    };
    let mut keys: Vec<String> = defaults.keys().cloned().collect();
    keys.extend(unset.iter().cloned());
    keys.sort();
    keys.dedup();
    let mut rows = Vec::new();
    for key in keys {
        let default_str = defaults
            .get(&key)
            .map(render)
            .unwrap_or_else(|| "unset".into());
        let (value, source) = if let Some(v) = p.as_ref().and_then(|m| m.get(&key)) {
            (render(v), "project")
        } else if let Some(v) = g.as_ref().and_then(|m| m.get(&key)) {
            (render(v), "global")
        } else {
            (default_str.clone(), "default")
        };
        rows.push(Row {
            key,
            default: default_str,
            value,
            source,
        });
    }
    Ok(rows)
}

/// Whether `value` differs from the default recorded for `key`. An unknown
/// key or a no-default (unset) key always differs, so setup offers it; an
/// equal value does not, so setup writes nothing. An unknown key stays
/// `true` on purpose: `config set` remains the validator.
pub fn differs(key: &str, value: &toml::Value) -> bool {
    let (defaults, unset) = example_inventory();
    if unset.iter().any(|k| k == key) {
        return true;
    }
    match defaults.get(key) {
        Some(d) => d != value,
        None => true,
    }
}

/// The global config file, beside the state root's other durable files.
fn global_config_path() -> PathBuf {
    crate::model_catalog::state_dir().join("config.toml")
}

/// The project config file: `$FNO_REPO_ROOT` when pinned, else the nearest
/// ancestor holding a `.git` dir, else the working directory.
fn project_config_path() -> PathBuf {
    if let Some(root) = std::env::var_os("FNO_REPO_ROOT").filter(|r| !r.is_empty()) {
        return PathBuf::from(root).join(".fno").join("config.toml");
    }
    let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    loop {
        if cwd.join(".git").exists() {
            return cwd.join(".fno").join("config.toml");
        }
        if !cwd.pop() {
            return PathBuf::from(".").join(".fno").join("config.toml");
        }
    }
}

/// The verb: print the inventory as aligned text, or a JSON array with
/// `--json`. A config file that cannot be parsed exits 2 naming the file.
pub fn run(tail: &[OsString]) -> i32 {
    let mut json = false;
    for a in tail {
        match a.to_str() {
            Some("--json") => json = true,
            _ => {
                eprintln!("usage: fno config get --defaults [--json]");
                return 2;
            }
        }
    }
    let global = global_config_path();
    let project = project_config_path();
    let rows = match inventory(Some(&global), Some(&project)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fno config get --defaults: {e}");
            return 2;
        }
    };
    if json {
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "key": r.key,
                    "default": r.default,
                    "value": r.value,
                    "source": r.source,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
    } else {
        println!("{:<44} {:<26} {:<26} source", "key", "default", "value");
        for r in &rows {
            println!(
                "{:<44} {:<26} {:<26} {}",
                r.key, r.default, r.value, r.source
            );
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!(
            "fno-cfg-defaults-{}-{tag}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn every_example_key_lists_with_default_source() {
        let rows = inventory(None, None).unwrap();
        assert!(rows.len() > 50);
        assert!(rows.iter().all(|r| r.source == "default"));
        let max_live = rows.iter().find(|r| r.key == "agents.max_live").unwrap();
        assert_eq!(max_live.default, "3");
    }

    #[test]
    fn optional_keys_print_unset() {
        let rows = inventory(None, None).unwrap();
        let idp = rows.iter().find(|r| r.key == "backlog.id_prefix").unwrap();
        assert_eq!(idp.default, "unset");
        assert_eq!(idp.value, "unset");
    }

    #[test]
    fn a_global_override_reports_global() {
        let dir = tempdir("global");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[branch]\nprefix = \"xx\"\n").unwrap();
        let rows = inventory(Some(&path), None).unwrap();
        let row = rows.iter().find(|r| r.key == "branch.prefix").unwrap();
        assert_eq!(row.source, "global");
        assert_eq!(row.default, "fno");
        assert_eq!(row.value, "xx");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn project_beats_global() {
        let dir = tempdir("project");
        let g = dir.join("global.toml");
        let p = dir.join("project.toml");
        std::fs::write(&g, "[branch]\nprefix = \"gg\"\n").unwrap();
        std::fs::write(&p, "[branch]\nprefix = \"pp\"\n").unwrap();
        let rows = inventory(Some(&g), Some(&p)).unwrap();
        let row = rows.iter().find(|r| r.key == "branch.prefix").unwrap();
        assert_eq!(row.source, "project");
        assert_eq!(row.value, "pp");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unparseable_config_names_the_file() {
        let dir = tempdir("bad");
        let path = dir.join("config.toml");
        std::fs::write(&path, "not [ valid toml").unwrap();
        let err = inventory(Some(&path), None).unwrap_err();
        assert!(err.contains(&path.display().to_string()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn differs_compares_against_the_recorded_default() {
        assert!(!differs(
            "guards.preset",
            &toml::Value::String("standard".into())
        ));
        assert!(differs(
            "guards.preset",
            &toml::Value::String("strict".into())
        ));
        assert!(differs(
            "backlog.id_prefix",
            &toml::Value::String("myproj".into())
        ));
        assert!(differs("no.such.key", &toml::Value::Boolean(true)));
    }

    #[test]
    fn classify_claims_only_the_defaults_flag() {
        let oss = |ps: &[&str]| ps.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(classify(&oss(&["config", "get", "--defaults"])).is_some());
        assert!(classify(&oss(&["config", "get", "--json", "--defaults"])).is_some());
        assert!(classify(&oss(&["config", "get"])).is_none());
        assert!(classify(&oss(&["config", "get", "--defaults", "branch.prefix"])).is_none());
        assert!(classify(&oss(&["config", "get", "branch.prefix"])).is_none());
    }
}
