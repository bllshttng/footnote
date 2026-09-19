//! Integration tests for the OpenCode installer: catalog naming agreement,
//! the install surface, idempotence, upgrade, the uninstall honesty rules,
//! and the bystander hazard. Every case runs against a scratch
//! OPENCODE_CONFIG_DIR and a fake footnote tree; the user's real config dir
//! is never touched.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use fno_agents::opencode_install::{
    command_file_name, install, installed_status, manifest_path, uninstall,
};
use fno_agents::provider::{opencode_run_tail, render_verb_seed};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Scratch {
    _guard: MutexGuard<'static, ()>,
    root: PathBuf,
    conf: PathBuf,
}

fn tmp(name: &str) -> PathBuf {
    let tid = format!("{:?}", std::thread::current().id());
    let dir = std::env::temp_dir().join(format!("fno-ocinst-{name}-{}-{tid}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn scratch(name: &str) -> Scratch {
    let guard = ENV_LOCK.lock().unwrap();
    let base = tmp(name);
    let root = base.join("root");
    let conf = base.join("conf");
    let state = base.join("state");
    for dir in [&root, &conf, &state] {
        std::fs::create_dir_all(dir).unwrap();
    }
    write_file(
        &root.join(".claude-plugin/plugin.json"),
        r#"{"name":"fno","version":"9.9.9"}"#,
    );
    write_file(
        &root.join("cli/src/fno/setup/assets/opencode/footnote.js"),
        "// footnote bridge v9\n",
    );
    write_file(
        &root.join("commands/target.md"),
        "---\ndescription: \"footnote target - the spine\"\n---\nbody\n",
    );
    write_file(
        &root.join("commands/pr.md"),
        "---\ndescription: \"footnote pr - open the PR\"\n---\nbody\n",
    );
    write_file(
        &root.join("skills/think/SKILL.md"),
        "---\ndescription: think through it\n---\nskill body\n",
    );
    write_file(&root.join("skills/think/patterns.md"), "patterns\n");
    write_file(
        &root.join("agents/archer.md"),
        "---\ndescription: TDD executor\nmodel: sonnet\n---\nArcher prompt body\n",
    );
    write_file(
        &root.join("agents/scout.md"),
        "---\ndescription: scout around\nmodel: zai/glm-5.3\n---\nScout prompt body\n",
    );
    // The plugin-root env hint outranks the pointer, so FNO_REPO_ROOT pins
    // the source to the fake tree; FNO_HOME deflects the pointer read.
    std::env::set_var("FNO_REPO_ROOT", &root);
    std::env::remove_var("CLAUDE_PLUGIN_ROOT");
    std::env::remove_var("CODEX_PLUGIN_ROOT");
    std::env::set_var("OPENCODE_CONFIG_DIR", &conf);
    std::env::set_var("FNO_RECLAIM_STATE_ROOT", &state);
    std::env::set_var("FNO_HOME", &state);
    Scratch {
        _guard: guard,
        root,
        conf,
    }
}

/// Install once and hand back the receipt.
fn installed(name: &str) -> Scratch {
    let s = scratch(name);
    let receipt = install(Path::new("/nonexistent-repo")).unwrap();
    assert_eq!(receipt.status, "installed", "kept: {:?}", receipt.kept);
    s
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn naming_agreement_across_renderers_and_generator() {
    // opencode_run_tail routes a seed at --command fno:think; render_verb_seed
    // keeps the /fno: spelling on the slash surface; the generator names the
    // file fno:think.md. One string, three renderers.
    assert_eq!(opencode_run_tail("/fno:think extra words")[1], "fno:think");
    assert_eq!(render_verb_seed("/fno:think", "opencode"), "/fno:think");
    assert_eq!(command_file_name("think"), "fno:think.md");
}

#[test]
fn install_writes_the_full_surface() {
    let s = installed("full-surface");
    for verb in ["fno:target.md", "fno:pr.md", "fno:think.md"] {
        assert!(
            s.conf.join("command").join(verb).is_file(),
            "{verb} missing"
        );
    }
    let target = read(&s.conf.join("command/fno:target.md"));
    assert!(target.contains("description: \"footnote target - the spine\""));
    assert!(target.contains("Load the footnote skill \"target\""));
    assert!(target.contains("$ARGUMENTS"));
    let archer = read(&s.conf.join("agent/fno:archer.md"));
    assert!(archer.contains("mode: subagent"));
    assert!(archer.contains("description: \"TDD executor\""));
    assert!(!archer.contains("model:"), "bare model must be dropped");
    assert!(archer.contains("Archer prompt body"));
    let scout = read(&s.conf.join("agent/fno:scout.md"));
    assert!(scout.contains("model: zai/glm-5.3"));
    assert!(read(&s.conf.join("skills/think/SKILL.md")).contains("skill body"));
    assert!(read(&s.conf.join("skills/think/patterns.md")).contains("patterns"));
    assert!(read(&s.conf.join("plugins/footnote.js")).contains("bridge v9"));
    let manifest: serde_json::Value = serde_json::from_str(&read(&manifest_path(&s.conf))).unwrap();
    assert_eq!(manifest["version"], "9.9.9");
    let files = manifest["files"].as_object().unwrap();
    assert!(files.contains_key("command/fno:target.md"));
    assert!(files.contains_key("agent/fno:archer.md"));
    assert!(files.contains_key("skills/think/SKILL.md"));
    assert!(files.contains_key("plugins/footnote.js"));
}

#[test]
fn idempotent_install_changes_no_mtime_and_no_manifest() {
    let s = installed("idempotent");
    let before = mtime(&s.conf.join("command/fno:target.md"));
    let manifest_before = read(&manifest_path(&s.conf));
    let receipt = install(Path::new("/nonexistent-repo")).unwrap();
    assert_eq!(receipt.written, 0);
    assert_eq!(receipt.skipped, 8);
    assert_eq!(mtime(&s.conf.join("command/fno:target.md")), before);
    assert_eq!(read(&manifest_path(&s.conf)), manifest_before);
}

#[test]
fn upgrade_removes_lost_verb_and_writes_new_one() {
    let s = installed("upgrade");
    let archer_before = mtime(&s.conf.join("agent/fno:archer.md"));
    std::fs::remove_file(s.root.join("commands/pr.md")).unwrap();
    write_file(
        &s.root.join("commands/review.md"),
        "---\ndescription: review it\n---\nbody\n",
    );
    let receipt = install(Path::new("/nonexistent-repo")).unwrap();
    assert!(!s.conf.join("command/fno:pr.md").exists());
    assert!(s.conf.join("command/fno:review.md").exists());
    assert!(receipt.removed >= 1);
    assert_eq!(mtime(&s.conf.join("agent/fno:archer.md")), archer_before);
}

#[test]
fn uninstall_keeps_user_edited_file_and_names_it() {
    let s = installed("uninstall-edit");
    write_file(&s.conf.join("command/fno:target.md"), "// user edit\n");
    let receipt = uninstall().unwrap();
    assert_eq!(receipt.status, "partial");
    assert!(receipt.kept.contains(&"command/fno:target.md".to_string()));
    assert_eq!(
        read(&s.conf.join("command/fno:target.md")),
        "// user edit\n"
    );
    assert!(!s.conf.join("command/fno:pr.md").exists());
    assert!(!manifest_path(&s.conf).exists());
}

#[test]
fn bystanders_and_user_config_survive_the_full_cycle() {
    let s = scratch("bystander");
    // The 42-directory hazard, as files: a decoy skill (in the singular
    // spelling OpenCode also scans), a decoy agent, a decoy command, and the
    // three config files footnote must never touch.
    write_file(&s.conf.join("skill/decoy/SKILL.md"), "decoy skill\n");
    write_file(&s.conf.join("agent/decoy.md"), "decoy agent\n");
    write_file(&s.conf.join("command/decoy.md"), "decoy command\n");
    write_file(
        &s.conf.join("opencode.json"),
        "{\n  \"theme\": \"decoy\"\n}\n",
    );
    write_file(
        &s.conf.join("package.json"),
        "{\n  \"name\": \"decoy\"\n}\n",
    );
    write_file(&s.conf.join("AGENTS.md"), "user agents md\n");
    let install_receipt = install(Path::new("/nonexistent-repo")).unwrap();
    assert_eq!(install_receipt.status, "installed");
    assert_eq!(
        read(&s.conf.join("opencode.json")),
        "{\n  \"theme\": \"decoy\"\n}\n"
    );
    assert_eq!(
        read(&s.conf.join("package.json")),
        "{\n  \"name\": \"decoy\"\n}\n"
    );
    assert_eq!(read(&s.conf.join("AGENTS.md")), "user agents md\n");
    assert_eq!(read(&s.conf.join("skill/decoy/SKILL.md")), "decoy skill\n");
    assert_eq!(read(&s.conf.join("agent/decoy.md")), "decoy agent\n");
    assert_eq!(read(&s.conf.join("command/decoy.md")), "decoy command\n");
    let receipt = uninstall().unwrap();
    assert_eq!(receipt.status, "uninstalled");
    assert_eq!(
        read(&s.conf.join("opencode.json")),
        "{\n  \"theme\": \"decoy\"\n}\n"
    );
    assert_eq!(
        read(&s.conf.join("package.json")),
        "{\n  \"name\": \"decoy\"\n}\n"
    );
    assert_eq!(read(&s.conf.join("AGENTS.md")), "user agents md\n");
    assert_eq!(read(&s.conf.join("skill/decoy/SKILL.md")), "decoy skill\n");
    assert_eq!(read(&s.conf.join("agent/decoy.md")), "decoy agent\n");
    assert_eq!(read(&s.conf.join("command/decoy.md")), "decoy command\n");
    assert!(!manifest_path(&s.conf).exists());
}

#[test]
fn install_refuses_when_no_source_resolves() {
    let _guard = ENV_LOCK.lock().unwrap();
    let base = tmp("refusal");
    let conf = base.join("conf");
    std::fs::create_dir_all(&conf).unwrap();
    std::env::remove_var("CLAUDE_PLUGIN_ROOT");
    std::env::remove_var("CODEX_PLUGIN_ROOT");
    std::env::remove_var("FNO_REPO_ROOT");
    std::env::set_var("FNO_HOME", base.join("empty-home"));
    std::env::set_var("FNO_RECLAIM_STATE_ROOT", base.join("empty-state"));
    std::env::set_var("OPENCODE_CONFIG_DIR", &conf);
    let err = install(Path::new(&base.join("not-a-repo")))
        .expect_err("install must refuse without a footnote tree");
    assert!(err.contains("no footnote tree"), "{err}");
    assert!(!conf.join("plugins/footnote.js").exists());
    assert!(!manifest_path(&base.join("conf")).exists());
}

#[test]
fn uninstall_refuses_without_manifest() {
    let _guard = ENV_LOCK.lock().unwrap();
    let base = tmp("no-manifest");
    std::env::set_var("FNO_RECLAIM_STATE_ROOT", base.join("state"));
    std::env::set_var("OPENCODE_CONFIG_DIR", base.join("conf"));
    let err = uninstall().expect_err("uninstall must refuse with no manifest");
    assert!(err.contains("no manifest"), "{err}");
}

fn mtime(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

#[test]
fn stale_install_is_named_when_the_source_version_moves() {
    let s = installed("stale");
    assert_eq!(installed_status()["status"], "installed");
    write_file(
        &s.root.join(".claude-plugin/plugin.json"),
        r#"{"name":"fno","version":"9.9.10"}"#,
    );
    let quick = installed_status();
    assert_eq!(quick["status"], "stale", "version drift must be named");
    assert_eq!(quick["source_version"], "9.9.10");
    assert_eq!(quick["version"], "9.9.9");
    // Reinstall converges on the new version.
    install(Path::new("/nonexistent-repo")).unwrap();
    assert_eq!(installed_status()["status"], "installed");
}

#[test]
fn agent_restrictions_follow_the_translator_contract() {
    let s = scratch("restriction-parity");
    write_file(
        &s.root.join("agents/reviewer.md"),
        "---\ndescription: reviews code\ndisallowedTools: [\"Write\", \"Edit\", \"Bash\"]\n---\nReviewer body\n",
    );
    write_file(
        &s.root.join("agents/allowlisted.md"),
        "---\ndescription: allowlist only\ntools: [\"Read\", \"Grep\"]\n---\nAllowlisted body\n",
    );
    install(Path::new("/nonexistent-repo")).unwrap();

    // The denylist carries into OpenCode's disable-only tools record.
    let reviewer = read(&s.conf.join("agent/fno:reviewer.md"));
    assert!(reviewer.contains("mode: subagent"));
    assert!(reviewer.contains("write: false"));
    assert!(reviewer.contains("edit: false"));
    assert!(reviewer.contains("bash: false"));

    // The allowlist cannot be expressed: the agent is skipped, never
    // installed unrestricted.
    assert!(
        !s.conf.join("agent/fno:allowlisted.md").exists(),
        "an allowlist-carrying agent must not install unrestricted"
    );
    let manifest: serde_json::Value = serde_json::from_str(&read(&manifest_path(&s.conf))).unwrap();
    let files = manifest["files"].as_object().unwrap();
    assert!(files.contains_key("agent/fno:reviewer.md"));
    assert!(!files.contains_key("agent/fno:allowlisted.md"));
}
