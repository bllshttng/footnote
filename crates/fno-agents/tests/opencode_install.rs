//! Integration tests for the OpenCode installer: catalog naming agreement,
//! the install surface, idempotence, upgrade, the uninstall honesty rules,
//! and the bystander hazard. Every case runs against a scratch
//! OPENCODE_CONFIG_DIR and a fake footnote tree; the user's real config dir
//! is never touched. The suite is shrink-only: each test carries one
//! contract, with scenarios folded inside it rather than as sibling
//! declarations.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use fno_agents::opencode_install::{
    command_file_name, install, installed_status, manifest_path, status_json, uninstall,
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

/// A stub `opencode` on PATH whose --version output the test controls, so
/// the contract classification is deterministic.
fn stub_opencode(dir: &Path, version: &str) {
    write_file(
        &dir.join("opencode"),
        &format!("#!/bin/sh\ncase \"$1\" in --version) echo {version};; esac\n"),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("opencode"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
}

fn set_path(bin_dir: &Path) {
    let old = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{old}", bin_dir.display()));
}

fn scratch(name: &str) -> Scratch {
    let guard = ENV_LOCK.lock().unwrap();
    let base = tmp(name);
    let root = base.join("root");
    let conf = base.join("conf");
    let state = base.join("state");
    let bin = base.join("bin");
    for dir in [&root, &conf, &state, &bin] {
        std::fs::create_dir_all(dir).unwrap();
    }
    stub_opencode(&bin, "1.14.50");
    set_path(&bin);
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
        &root.join("commands/ship.md"),
        "---\ndescription: \"footnote ship - the delivery umbrella\"\n---\nbody\n",
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

/// The full installed surface and the idempotence contract over it, plus
/// binary targeting: `FNO_OPENCODE_BIN` names the opencode the install
/// classifies and asks for its root; the binary's `debug paths` config row
/// chooses the root over the XDG fallback; `OPENCODE_CONFIG_DIR` outranks the
/// binary; a binary that answers nothing still renders 2.x and names the
/// version unknown in the receipt. Naming agreement leads: opencode_run_tail
/// routes a seed at --command fno:think; render_verb_seed keeps the /fno:
/// spelling on the slash surface; the generator names the file fno:think.md.
/// One string, three renderers; the surface below is named by the same rule.
#[test]
fn install_targets_the_binary_writes_the_full_surface_and_reinstall_is_a_noop() {
    assert_eq!(opencode_run_tail("/fno:think extra words")[1], "fno:think");
    assert_eq!(render_verb_seed("/fno:think", "opencode"), "/fno:think");
    assert_eq!(command_file_name("think"), "fno:think.md");
    let s = installed("full-surface");
    for verb in ["fno:target.md", "fno:ship.md", "fno:think.md"] {
        assert!(
            s.conf.join("commands").join(verb).is_file(),
            "{verb} missing"
        );
    }
    let target = read(&s.conf.join("commands/fno:target.md"));
    assert!(target.contains("description: \"footnote target - the spine\""));
    assert!(target.contains("Load the footnote skill \"target\""));
    assert!(target.contains("$ARGUMENTS"));
    let archer = read(&s.conf.join("agents/fno:archer.md"));
    assert!(archer.contains("mode: subagent"));
    assert!(archer.contains("description: \"TDD executor\""));
    assert!(!archer.contains("model:"), "bare model must be dropped");
    assert!(archer.contains("Archer prompt body"));
    let scout = read(&s.conf.join("agents/fno:scout.md"));
    assert!(scout.contains("model: zai/glm-5.3"));
    assert!(read(&s.conf.join("skills/think/SKILL.md")).contains("skill body"));
    assert!(read(&s.conf.join("skills/think/patterns.md")).contains("patterns"));
    assert!(read(&s.conf.join("plugins/footnote.js")).contains("bridge v9"));
    let manifest: serde_json::Value = serde_json::from_str(&read(&manifest_path(&s.conf))).unwrap();
    assert_eq!(manifest["version"], "9.9.9");
    assert_eq!(manifest["opencode_contract"], "1.x");
    let files = manifest["files"].as_object().unwrap();
    assert!(files.contains_key("commands/fno:target.md"));
    assert!(files.contains_key("agents/fno:archer.md"));
    assert!(files.contains_key("skills/think/SKILL.md"));
    assert!(files.contains_key("plugins/footnote.js"));
    let receipt = serde_json::to_value(install(Path::new("/nonexistent-repo")).unwrap()).unwrap();
    assert_eq!(receipt["contract"], "1.x");
    assert_eq!(receipt["opencode_version"], "1.14.50");
    // Idempotence: written 0, no mtime churn, manifest untouched.
    let before = mtime(&s.conf.join("commands/fno:target.md"));
    let manifest_before = read(&manifest_path(&s.conf));
    let receipt = install(Path::new("/nonexistent-repo")).unwrap();
    assert_eq!(receipt.written, 0);
    assert_eq!(receipt.skipped, 8);
    assert_eq!(mtime(&s.conf.join("commands/fno:target.md")), before);
    assert_eq!(read(&manifest_path(&s.conf)), manifest_before);
    // Drop the env lock: the targeting legs re-scratch, and the lock is
    // held by the live Scratch until it drops.
    drop(s);
    // (a) The binary's debug paths chooses the root; a v2 binary renders
    // the `permissions` list there and the receipt names the binary, the
    // root source, and the reported version.
    {
        let s = scratch("target-binary");
        // A space in the root name: the debug-paths parse must carry the
        // whole rest of the row, not just the next whitespace token.
        let conf_v2 = s.root.parent().unwrap().join("conf v2");
        std::fs::create_dir_all(&conf_v2).unwrap();
        write_file(
            &s.root.join("agents/allow.md"),
            "---\ndescription: allowlisted\ntools: [\"Read\", \"Bash\"]\n---\nAllowlisted body\n",
        );
        let stub = s.root.parent().unwrap().join("opencode-v2-stub");
        let row = format!("config {}", conf_v2.display());
        write_file(
            &stub,
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'opencode v2.0.19';;\n  debug) if [ \"$2\" = paths ]; then echo 'data   {row}'\necho '{row}'; fi;;\nesac\n"
            ),
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("FNO_OPENCODE_BIN", &stub);
        std::env::remove_var("OPENCODE_CONFIG_DIR");
        let receipt = install(Path::new("/nonexistent-repo")).unwrap();
        assert_eq!(receipt.contract, "2.x");
        assert_eq!(
            receipt.opencode_version.as_deref(),
            Some("opencode v2.0.19")
        );
        assert!(receipt.opencode_bin.ends_with("opencode-v2-stub"));
        assert_eq!(receipt.config_dir, conf_v2.display().to_string());
        assert_eq!(receipt.config_dir_source, "debug paths");
        let rendered = read(&conf_v2.join("agents/fno:allow.md"));
        assert!(
            rendered.contains("permissions:"),
            "v2 agent files carry a permissions list: {rendered}"
        );
        assert!(conf_v2.join("plugins/footnote.js").is_file());
        // (c) OPENCODE_CONFIG_DIR outranks the binary's answer.
        std::env::set_var("OPENCODE_CONFIG_DIR", &s.conf);
        let receipt = install(Path::new("/nonexistent-repo")).unwrap();
        assert_eq!(receipt.config_dir, s.conf.display().to_string());
        assert_eq!(receipt.config_dir_source, "env");
        std::env::remove_var("FNO_OPENCODE_BIN");
    }
    // (b) A binary that answers nothing: 2.x renders, version unknown,
    // the XDG fallback holds the files.
    {
        let s = scratch("target-missing");
        let conf_fallback = s.root.parent().unwrap().join("conf-fallback");
        std::fs::create_dir_all(&conf_fallback).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", s.root.parent().unwrap().join("xdg"));
        std::env::set_var(
            "FNO_OPENCODE_BIN",
            s.root.parent().unwrap().join("no-such-opencode"),
        );
        std::env::remove_var("OPENCODE_CONFIG_DIR");
        let receipt = install(Path::new("/nonexistent-repo")).unwrap();
        assert_eq!(receipt.contract, "2.x");
        assert_eq!(receipt.opencode_version, None, "version unknown is named");
        assert_eq!(receipt.config_dir_source, "fallback");
        assert!(receipt.config_dir.ends_with("xdg/opencode"));
        std::env::remove_var("FNO_OPENCODE_BIN");
        std::env::remove_var("XDG_CONFIG_HOME");
    }
}

/// The upgrade contract: a lost verb is removed, a new one written, a lost
/// file the user edited since the install is kept and named, and untouched
/// files keep their mtime.
#[test]
fn upgrade_removes_lost_writes_new_keeps_edited() {
    let s = installed("upgrade");
    let archer_before = mtime(&s.conf.join("agents/fno:archer.md"));
    // A lost verb footnote still owns: removed.
    std::fs::remove_file(s.root.join("commands/ship.md")).unwrap();
    write_file(
        &s.root.join("commands/review.md"),
        "---\ndescription: review it\n---\nbody\n",
    );
    // A lost verb the user edited: theirs.
    write_file(&s.conf.join("commands/fno:target.md"), "// user edit\n");
    std::fs::remove_file(s.root.join("commands/target.md")).unwrap();
    let receipt = install(Path::new("/nonexistent-repo")).unwrap();
    assert!(!s.conf.join("commands/fno:ship.md").exists());
    assert!(s.conf.join("commands/fno:review.md").exists());
    assert!(receipt.removed >= 1);
    assert_eq!(
        read(&s.conf.join("commands/fno:target.md")),
        "// user edit\n"
    );
    assert!(receipt.kept.contains(&"commands/fno:target.md".to_string()));
    let manifest: serde_json::Value = serde_json::from_str(&read(&manifest_path(&s.conf))).unwrap();
    assert!(!manifest["files"]
        .as_object()
        .unwrap()
        .contains_key("commands/fno:target.md"));
    assert_eq!(mtime(&s.conf.join("agents/fno:archer.md")), archer_before);
}

/// The uninstall honesty contract: edited files are kept and named, owned
/// files are removed, the manifest goes last, and a config dir with no
/// manifest refuses outright.
#[test]
fn uninstall_keeps_edited_removes_owned_and_refuses_without_manifest() {
    let s = installed("uninstall-edit");
    write_file(&s.conf.join("commands/fno:target.md"), "// user edit\n");
    let receipt = uninstall().unwrap();
    assert_eq!(receipt.status, "partial");
    assert!(receipt.kept.contains(&"commands/fno:target.md".to_string()));
    assert_eq!(
        read(&s.conf.join("commands/fno:target.md")),
        "// user edit\n"
    );
    assert!(!s.conf.join("commands/fno:ship.md").exists());
    assert!(!manifest_path(&s.conf).exists());
    // No manifest, no removal: the second uninstall refuses.
    let err = uninstall().expect_err("uninstall must refuse with no manifest");
    assert!(err.contains("no manifest"), "{err}");
}

#[test]
fn bystanders_and_user_config_survive_the_full_cycle() {
    let s = scratch("bystander");
    // The 42-directory hazard, as files: a decoy skill (in the singular
    // spelling OpenCode also scans), a decoy agent, a decoy command, and the
    // three config files footnote must never touch.
    write_file(&s.conf.join("skill/decoy/SKILL.md"), "decoy skill\n");
    write_file(&s.conf.join("agents/decoy.md"), "decoy agent\n");
    write_file(&s.conf.join("commands/decoy.md"), "decoy command\n");
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
    assert_eq!(read(&s.conf.join("agents/decoy.md")), "decoy agent\n");
    assert_eq!(read(&s.conf.join("commands/decoy.md")), "decoy command\n");
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
    assert_eq!(read(&s.conf.join("agents/decoy.md")), "decoy agent\n");
    assert_eq!(read(&s.conf.join("commands/decoy.md")), "decoy command\n");
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

fn mtime(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

/// The staleness contract: version drift and an opencode contract change
/// (1.x manifest, opencode now 2.x) both read stale, and a reinstall
/// converges back to installed.
#[test]
fn stale_reads_version_drift_and_contract_change() {
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
    // An opencode upgrade across 2.0.0 after an install reads stale too:
    // the recorded contract no longer matches the reported one.
    let bin = s.root.parent().unwrap().join("bin2");
    std::fs::create_dir_all(&bin).unwrap();
    stub_opencode(&bin, "2.0.3");
    set_path(&bin);
    assert_eq!(installed_status()["status"], "stale");
    assert_eq!(status_json()["status"], "stale");
}

/// The restriction render contract, under both contracts: a denylist
/// carries into 1.x's permission map as deny entries; an allowlist
/// installs as deny-all + allows, never unrestricted; a 2.x stub opencode
/// flips the render to a `permissions` rule list with shell/subagent
/// names; an allowlist that maps to nothing skips the agent.
#[test]
fn agent_restrictions_render_as_permission_records() {
    let s = scratch("restriction-parity");
    write_file(
        &s.root.join("agents/reviewer.md"),
        "---\ndescription: reviews code\ndisallowedTools: [\"Write\", \"Edit\", \"Bash\"]\n---\nReviewer body\n",
    );
    write_file(
        &s.root.join("agents/allowlisted.md"),
        "---\ndescription: allowlist only\ntools: [\"Read\", \"Grep\", \"Bash\", \"Skill\", \"Write\", \"Edit\"]\n---\nAllowlisted body\n",
    );
    write_file(
        &s.root.join("agents/unmappable.md"),
        "---\ndescription: nothing maps\ntools: [\"NotebookEdit\"]\n---\nUnmappable body\n",
    );
    install(Path::new("/nonexistent-repo")).unwrap();

    // The denylist carries into the 1.x permission map as deny entries.
    let reviewer = read(&s.conf.join("agents/fno:reviewer.md"));
    assert!(reviewer.contains("mode: subagent"));
    assert!(reviewer.contains("permission:\n  edit: deny"));
    assert!(reviewer.contains("bash: deny"));

    // The allowlist installs restricted: deny-all first, allows after.
    // Write and Edit both map to the edit key: the render must carry it
    // once, not as a duplicate YAML mapping key.
    let allowlisted = read(&s.conf.join("agents/fno:allowlisted.md"));
    assert!(allowlisted.contains("permission:\n  \"*\": deny\n  read: allow\n  grep: allow\n  bash: allow\n  skill: allow\n  edit: allow\n"));
    assert_eq!(
        allowlisted.matches("edit: allow").count(),
        1,
        "Write and Edit collapse to one edit key"
    );

    // An allowlist that maps to nothing skips the agent.
    assert!(
        !s.conf.join("agents/fno:unmappable.md").exists(),
        "an allowlist mapping to nothing must not install unrestricted"
    );
    let manifest: serde_json::Value = serde_json::from_str(&read(&manifest_path(&s.conf))).unwrap();
    let files = manifest["files"].as_object().unwrap();
    assert!(files.contains_key("agents/fno:reviewer.md"));
    assert!(files.contains_key("agents/fno:allowlisted.md"));
    assert!(!files.contains_key("agents/fno:unmappable.md"));

    // The same allowlist under a 2.x stub opencode renders the `permissions`
    // rule list with shell (bash's 2.x name) and subagent (task's).
    let bin = s.root.parent().unwrap().join("bin2");
    std::fs::create_dir_all(&bin).unwrap();
    stub_opencode(&bin, "2.0.3");
    set_path(&bin);
    write_file(
        &s.root.join("agents/allowlisted.md"),
        "---\ndescription: allowlist only\ntools: [\"Read\", \"Bash\", \"Task\", \"Skill\"]\n---\nAllowlisted body\n",
    );
    install(Path::new("/nonexistent-repo")).unwrap();
    let rendered = read(&s.conf.join("agents/fno:allowlisted.md"));
    assert!(rendered
        .contains("permissions:\n  - action: \"*\"\n    resource: \"*\"\n    effect: deny\n"));
    assert!(rendered.contains("  - action: read\n    resource: \"*\"\n    effect: allow\n"));
    assert!(rendered.contains("  - action: shell\n    resource: \"*\"\n    effect: allow\n"));
    assert!(rendered.contains("  - action: subagent\n    resource: \"*\"\n    effect: allow\n"));
    let manifest: serde_json::Value = serde_json::from_str(&read(&manifest_path(&s.conf))).unwrap();
    assert_eq!(manifest["opencode_contract"], "2.x");
    assert_eq!(installed_status()["status"], "installed");
}

/// The legacy-bridge adoption contract: a pre-manifest footnote.js whose
/// first line is the shipped header is footnote's own install - backed up,
/// replaced, named in replaced_legacy; any other pre-manifest bridge is
/// the user's, kept, named, status partial.
#[test]
fn legacy_bridge_adoption_backs_up_and_replaces() {
    {
        let s = scratch("legacy-adopt");
        write_file(
            &s.conf.join("plugins/footnote.js"),
            "// footnote bridge v9\nold bridge body\n",
        );
        let receipt = install(Path::new("/nonexistent-repo")).unwrap();
        assert_eq!(
            read(&s.conf.join("plugins/footnote.js")),
            "// footnote bridge v9\n"
        );
        assert_eq!(receipt.replaced_legacy.len(), 1, "named in the receipt");
        let backup = &receipt.replaced_legacy[0].backup;
        assert!(backup.contains(".fno-backup-"), "{backup}");
        assert_eq!(
            read(&s.conf.join(backup)),
            "// footnote bridge v9\nold bridge body\n"
        );
        // The first Scratch drops here: it holds the env lock, and the second
        // scratch below would deadlock against itself otherwise.
    }

    // A bridge with a different first line is the user's: kept, partial.
    let s = scratch("legacy-foreign");
    write_file(
        &s.conf.join("plugins/footnote.js"),
        "// totally different plugin\nbody\n",
    );
    let receipt = install(Path::new("/nonexistent-repo")).unwrap();
    assert_eq!(receipt.status, "partial");
    assert!(receipt.kept.contains(&"plugins/footnote.js".to_string()));
    assert_eq!(
        read(&s.conf.join("plugins/footnote.js")),
        "// totally different plugin\nbody\n"
    );
}
