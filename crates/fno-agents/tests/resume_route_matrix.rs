use fno_agents::harness_capabilities::HarnessContract;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

const HARNESSES: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "agy",
    "pi",
    "cursor-agent",
    "grok",
    "gemini",
];

#[derive(Clone)]
struct Row {
    harness: &'static str,
    name: String,
    substrate: &'static str,
    short_id: Option<String>,
    command_id: String,
    resume_id: String,
    mux_ref: bool,
}

struct Fixture {
    root: TempDir,
    home: PathBuf,
    bins: PathBuf,
    rows: Vec<Row>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("agents");
        let bins = root.path().join("bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bins).unwrap();
        for harness in HARNESSES {
            write_executable(
                &bins.join(harness),
                "#!/bin/sh\necho fake-executed\nprintf 'arg=<%s>\\n' \"$@\"\n",
            );
        }
        let mut rows = Vec::new();
        let mut entries = Vec::new();
        for (index, harness) in HARNESSES.iter().enumerate() {
            for substrate in ["thread", "pane"] {
                let row = make_row(*harness, substrate, index, false);
                entries.push(row_json(&row, root.path()));
                rows.push(row);
            }
        }
        let pane_ref = make_row("pi", "pane", HARNESSES.len(), true);
        entries.push(row_json(&pane_ref, root.path()));
        rows.push(pane_ref);
        write_registry(&home, &entries);

        Self {
            root,
            home,
            bins,
            rows,
        }
    }

    fn row(&self, harness: &str, substrate: &str, mux_ref: bool) -> &Row {
        self.rows
            .iter()
            .find(|row| {
                row.harness == harness && row.substrate == substrate && row.mux_ref == mux_ref
            })
            .expect("fixture row exists")
    }

    fn events(&self) -> String {
        fs::read_to_string(self.root.path().join("events.jsonl")).unwrap_or_default()
    }

    fn write_registry_entries(&self, entries: &[Value]) {
        write_registry(&self.home, entries);
    }
}

fn make_row(harness: &'static str, substrate: &'static str, index: usize, mux_ref: bool) -> Row {
    let mode = if substrate == "thread" {
        "thread"
    } else {
        "pane"
    };
    let name = format!("resume-{harness}-{mode}-{}", usize::from(mux_ref));
    let ordinal = index * 2 + usize::from(substrate != "thread") + usize::from(mux_ref) * 2;
    let generic_id = format!("resume-{harness}-{mode}-{}", usize::from(mux_ref));
    let (command_id, resume_id) = if harness == "claude" {
        let uuid = format!("00000000-0000-4000-8000-{:012x}", 100 + ordinal);
        (uuid.clone(), uuid)
    } else {
        (generic_id.clone(), generic_id)
    };
    Row {
        harness,
        name,
        substrate,
        short_id: (harness == "claude").then(|| format!("cafe{ordinal:04x}")),
        command_id,
        resume_id,
        mux_ref,
    }
}

fn row_json(row: &Row, cwd: &Path) -> Value {
    let mut entry = json!({
        "name": row.name,
        "harness": row.harness,
        "substrate": row.substrate,
        "status": "exited",
        "exited_at": "2026-09-23T00:00:00Z",
        "cwd": cwd,
        "log_path": cwd.join(format!("{}.log", row.name)),
        "created_at": "2026-09-22T00:00:00Z",
        "harness_session_id": row.resume_id,
    });
    if let Some(short_id) = &row.short_id {
        entry["short_id"] = json!(short_id);
    }
    if row.harness == "claude" {
        entry["claude_session_uuid"] = json!(row.resume_id);
    }
    if row.mux_ref {
        entry["mux"] = json!({"session": "resume-probe", "pane_id": 7});
    }
    entry
}

fn write_registry(home: &Path, entries: &[Value]) {
    fs::write(
        home.join("registry.json"),
        serde_json::to_vec(&json!({
            "schema_version": fno_agents::state::REGISTRY_SCHEMA_VERSION,
            "agents": entries,
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn run_null(fixture: &Fixture, row: &Row, extra: &[&str], path: &Path) -> Output {
    run_null_with_env(fixture, row, extra, path, &[])
}

fn run_null_with_env(
    fixture: &Fixture,
    row: &Row,
    extra: &[&str],
    path: &Path,
    extra_env: &[(&str, &str)],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fno-agents"));
    command
        .args(["resume", &row.command_id])
        .args(extra)
        .env_clear()
        .envs(fno_agents::test_run::self_owner_env())
        .env("FNO_AGENTS_HOME", &fixture.home)
        .env("HOME", fixture.root.path())
        .env("PATH", path)
        .stdin(Stdio::null());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().expect("fno-agents starts")
}

fn run_terminal(fixture: &Fixture, row: &Row, path: &Path) -> (u32, String) {
    run_terminal_with_env(fixture, row, path, &[])
}

fn run_terminal_with_env(
    fixture: &Fixture,
    row: &Row,
    path: &Path,
    extra_env: &[(&str, &str)],
) -> (u32, String) {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_fno-agents"));
    command.args(["resume", row.command_id.as_str()]);
    command.env_clear();
    for (key, value) in fno_agents::test_run::self_owner_env() {
        command.env(key, value);
    }
    command.env("FNO_AGENTS_HOME", &fixture.home);
    command.env("HOME", fixture.root.path());
    command.env("PATH", path);
    command.env("TERM", "xterm-256color");
    command.cwd(fixture.root.path());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut output = String::new();
    reader.read_to_string(&mut output).unwrap();
    let status = child.wait().unwrap();
    (status.exit_code(), output)
}

#[test]
fn each_contract_harness_prints_its_interactive_resume_form() {
    let fixture = Fixture::new();
    let contract = HarnessContract::packaged().unwrap();
    for row in &fixture.rows[..HARNESSES.len() * 2] {
        let output = run_null(&fixture, row, &["--print-command"], &fixture.bins);
        assert!(
            output.status.success(),
            "{} {} print refused: {}",
            row.harness,
            row.substrate,
            String::from_utf8_lossy(&output.stderr)
        );
        let form_session_id = if row.harness == "claude" {
            &row.resume_id
        } else {
            row.short_id.as_deref().unwrap_or(&row.resume_id)
        };
        let rendered = contract
            .render_session_argv_raw(row.harness, "interactive_resume", Some(form_session_id))
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        for token in rendered {
            assert!(
                stdout.contains(&token),
                "{} {} print omitted token {token:?}: {stdout}",
                row.harness,
                row.substrate
            );
        }
    }
}

#[test]
fn refused_thread_routes_name_the_contract_reason_and_write_no_resume_event() {
    let fixture = Fixture::new();
    let empty_path = fixture.root.path().join("empty-bin");
    fs::create_dir_all(&empty_path).unwrap();
    let contract = HarnessContract::packaged().unwrap();
    for harness in ["opencode", "gemini"] {
        let row = fixture.row(harness, "thread", false);
        let output = run_null(&fixture, row, &[], &empty_path);
        assert_eq!(output.status.code(), Some(13), "{harness}: {output:?}");
        let line = String::from_utf8_lossy(&output.stderr);
        let refusal = contract.conversion(harness).unwrap().refusal;
        assert!(line.contains(&refusal), "{harness}: {line}");
    }
    for harness in ["agy", "pi", "cursor-agent", "grok"] {
        let row = fixture.row(harness, "thread", false);
        let output = run_null(&fixture, row, &[], &empty_path);
        assert_eq!(output.status.code(), Some(13), "{harness}: {output:?}");
        let line = String::from_utf8_lossy(&output.stderr);
        assert!(
            line.contains(&format!(
                "the {harness} keeper lane has no revival for an exited thread"
            )),
            "{harness}: {line}"
        );
        for token in contract
            .render_session_argv_raw(harness, "interactive_resume", Some(&row.resume_id))
            .unwrap()
        {
            assert!(
                line.contains(&token),
                "{harness}: missing {token:?}: {line}"
            );
        }
    }
    assert!(!fixture.events().contains("agent_resumed"));
}

#[test]
fn nonterminal_rows_refuse_instead_of_relaunching_and_terminal_rows_exec() {
    let fixture = Fixture::new();
    for row in fixture
        .rows
        .iter()
        .take(HARNESSES.len() * 2)
        .skip(1)
        .step_by(2)
    {
        let output = run_null(&fixture, row, &[], &fixture.bins);
        assert_eq!(
            output.status.code(),
            Some(13),
            "{}: {output:?}",
            row.harness
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&format!(
                "the {} resume form is a terminal program and this caller has no terminal; run fno agents resume {} from a terminal",
                row.harness, row.command_id
            )),
            "{}: {}",
            row.harness,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let pane_row = fixture.row("pi", "pane", true);
    let output = run_null(&fixture, pane_row, &[], &fixture.bins);
    assert_eq!(output.status.code(), Some(13));
    let refusal = String::from_utf8_lossy(&output.stderr);
    assert!(refusal.contains("still records a mux pane"), "{refusal}");
    assert!(refusal.contains(&format!(
        "fno agents resume {} from a terminal",
        pane_row.command_id
    )));
    assert!(!fixture.events().contains("agent_resumed"));

    let (code, terminal_output) = run_terminal(&fixture, pane_row, &fixture.bins);
    assert_eq!(code, 0, "{terminal_output}");
    assert!(
        terminal_output.contains("fake-executed"),
        "{terminal_output}"
    );
    assert!(fixture.events().contains("agent_resumed"));
}

#[test]
fn terminal_missing_binary_and_rowless_session_refusals_are_actionable() {
    let fixture = Fixture::new();
    let empty_path = fixture.root.path().join("no-provider-bins");
    fs::create_dir_all(&empty_path).unwrap();
    write_executable(
        &empty_path.join("fno"),
        "#!/bin/sh\n[ \"$1\" = agents ] && [ \"$2\" = heal-token ] && exit 13\nexit 2\n",
    );
    let row = fixture.row("opencode", "pane", false);
    let (code, output) = run_terminal(&fixture, row, &empty_path);
    assert_eq!(code, 14, "{output}");
    assert!(output.contains("opencode CLI not on PATH"), "{output}");

    fixture.write_registry_entries(&[]);
    let session_id = "00000000-0000-4000-8000-00000000dead";
    let output = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .args(["resume", session_id])
        .env_clear()
        .envs(fno_agents::test_run::self_owner_env())
        .env("FNO_AGENTS_HOME", &fixture.home)
        .env("HOME", fixture.root.path())
        .env("PATH", &empty_path)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(13));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("fno agents adopt {session_id}")),
        "{stderr}"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .args(["resume", "ghost"])
        .env_clear()
        .envs(fno_agents::test_run::self_owner_env())
        .env("FNO_AGENTS_HOME", &fixture.home)
        .env("HOME", fixture.root.path())
        .env("PATH", &empty_path)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(13));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("pass a full session id to resume an orphaned session"),
        "{stderr}"
    );
    assert!(!stderr.contains("fno agents adopt"), "{stderr}");
}

#[test]
fn codex_print_command_keeps_route_args_and_masks_route_key_without_side_effects() {
    let fixture = Fixture::new();
    let row = fixture.row("codex", "thread", false).clone();
    let mut entries = fixture
        .rows
        .iter()
        .map(|row| row_json(row, fixture.root.path()))
        .collect::<Vec<_>>();
    let entry = entries
        .iter_mut()
        .find(|entry| entry["name"] == row.name)
        .unwrap();
    entry["route_provider_id"] = json!("zai-openai");
    entry["model_name"] = json!("glm-route-test");
    entry["node"] = json!("resume-node-test");
    fixture.write_registry_entries(&entries);

    let config_dir = fixture.root.path().join(".fno");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        "[model_routing.providers.zai-openai]\nprotocol = \"openai\"\nbase_url = \"https://example.invalid/v1\"\napi_key_env = \"FNO_TEST_ZAI_KEY\"\n",
    )
    .unwrap();

    let output = run_null_with_env(
        &fixture,
        &row,
        &["--print-command"],
        &fixture.bins,
        &[("FNO_TEST_ZAI_KEY", "must-not-print-this-key")],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("model_providers.zai-openai"), "{stdout}");
    assert!(
        stdout.contains("model_provider=") && stdout.contains("zai-openai"),
        "{stdout}"
    );
    assert!(
        stdout.contains("model=") && stdout.contains("glm-route-test"),
        "{stdout}"
    );
    assert!(stdout.contains("exec sh -c "), "{stdout}");
    assert!(
        stdout.contains(&format!("FNO_AGENT_SELF={}", row.name)),
        "{stdout}"
    );
    assert!(stdout.contains("FNO_AGENT_HARNESS=codex"), "{stdout}");
    assert!(stdout.contains("FNO_NODE=resume-node-test"), "{stdout}");
    let route_provider = stdout.find("FNO_ROUTE_PROVIDER=zai-openai").unwrap();
    let codex_executable = stdout.rfind("codex").unwrap();
    assert!(route_provider < codex_executable, "{stdout}");
    assert!(
        stdout.contains("FNO_TEST_ZAI_KEY=<from config>"),
        "{stdout}"
    );
    assert!(!stdout.contains("must-not-print-this-key"), "{stdout}");
    assert!(!stdout.contains("fake-executed"), "{stdout}");
    assert!(fixture.events().is_empty(), "{}", fixture.events());

    let entry = entries
        .iter_mut()
        .find(|entry| entry["name"] == row.name)
        .unwrap();
    entry["route_provider_id"] = json!("missing-provider");
    fixture.write_registry_entries(&entries);
    let output = run_null(&fixture, &row, &["--print-command"], &fixture.bins);
    assert_eq!(
        output.status.code(),
        Some(fno_agents::reentry::REENTRY_REFUSED_EXIT)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("was launched on codex route missing-provider"),
        "{stderr}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fake-executed"));
    assert!(fixture.events().is_empty(), "{}", fixture.events());
}

#[test]
fn claude_print_command_keeps_reentry_settings_and_env_without_truth_or_side_effects() {
    let fixture = Fixture::new();
    let row = fixture.row("claude", "pane", false).clone();
    let settings = fixture.root.path().join("route-settings.json");
    fs::write(
        &settings,
        r#"{"env":{"ANTHROPIC_BASE_URL":"https://example.invalid","FNO_ROUTE_PROVIDER":"routed"}}"#,
    )
    .unwrap();
    let config_dir = fixture.root.path().join("claude-config");
    fs::create_dir_all(&config_dir).unwrap();
    let account_log = fixture.root.path().join("fno-calls.log");
    write_executable(
        &fixture.bins.join("fno"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$FNO_TEST_ACCOUNT_LOG\"\nif [ \"$1 $2 $3 $4 $5\" = 'config accounts show makers --print-binding' ]; then printf 'CLAUDE_CONFIG_DIR=%s\\n' \"$FNO_TEST_CLAUDE_CONFIG\"; exit 0; fi\nexit 17\n",
    );
    let mut entries = fixture
        .rows
        .iter()
        .map(|row| row_json(row, fixture.root.path()))
        .collect::<Vec<_>>();
    let entry = entries
        .iter_mut()
        .find(|entry| entry["name"] == row.name)
        .unwrap();
    entry["route_settings_path"] = json!(settings);
    entry["launch_account"] = json!("makers");
    entry["node"] = json!("resume-node-test");
    entry.as_object_mut().unwrap().remove("short_id");
    fixture.write_registry_entries(&entries);

    let config_dir = config_dir.to_string_lossy().into_owned();
    let account_log = account_log.to_string_lossy().into_owned();
    let output = run_null_with_env(
        &fixture,
        &row,
        &["--print-command"],
        &fixture.bins,
        &[
            ("FNO_TEST_CLAUDE_CONFIG", &config_dir),
            ("FNO_TEST_ACCOUNT_LOG", &account_log),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--settings"), "{stdout}");
    assert!(stdout.contains(settings.to_str().unwrap()), "{stdout}");
    assert!(
        stdout.contains(&format!("CLAUDE_CONFIG_DIR={config_dir}")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "exec env FNO_AGENT_SELF={} FNO_AGENT_HARNESS=claude FNO_NODE=resume-node-test",
            row.name
        )),
        "{stdout}"
    );
    let config_env = stdout
        .find(&format!("CLAUDE_CONFIG_DIR={config_dir}"))
        .unwrap();
    let claude_executable = stdout.rfind(" claude ").unwrap();
    assert!(config_env < claude_executable, "{stdout}");
    assert!(!stdout.contains("fake-executed"), "{stdout}");
    assert_eq!(
        fs::read_to_string(&account_log).unwrap(),
        "config accounts show makers --print-binding\n"
    );
    assert!(fixture.events().is_empty(), "{}", fixture.events());

    let entry = entries
        .iter_mut()
        .find(|entry| entry["name"] == row.name)
        .unwrap();
    entry["route_settings_path"] = json!(fixture.root.path().join("missing-settings.json"));
    fixture.write_registry_entries(&entries);
    let output = run_null_with_env(
        &fixture,
        &row,
        &["--print-command"],
        &fixture.bins,
        &[
            ("FNO_TEST_CLAUDE_CONFIG", &config_dir),
            ("FNO_TEST_ACCOUNT_LOG", &account_log),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let contract = HarnessContract::packaged().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for token in contract
        .render_session_argv_raw("claude", "interactive_resume", Some(&row.resume_id))
        .unwrap()
    {
        assert!(stdout.contains(&token), "missing {token:?}: {stdout}");
    }
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("printing the declared contract command only"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!stdout.contains("--settings"), "{stdout}");
    assert!(!stdout.contains("fake-executed"), "{stdout}");
    assert_eq!(
        fs::read_to_string(&account_log).unwrap(),
        "config accounts show makers --print-binding\n"
    );
    assert!(fixture.events().is_empty(), "{}", fixture.events());
}

#[test]
fn claude_print_command_uses_transcript_resolved_cwd() {
    let fixture = Fixture::new();
    let row = fixture.row("claude", "thread", false).clone();
    let root = fixture.root.path().canonicalize().unwrap();
    let stale_cwd = root.join("removed-worktree");
    let mut entries = fixture
        .rows
        .iter()
        .map(|row| row_json(row, fixture.root.path()))
        .collect::<Vec<_>>();
    let entry = entries
        .iter_mut()
        .find(|entry| entry["name"] == row.name)
        .unwrap();
    entry["cwd"] = json!(stale_cwd);
    fixture.write_registry_entries(&entries);

    let transcript = root
        .join(".claude/projects/resume-cwd-test")
        .join(format!("{}.jsonl", row.resume_id));
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(
        &transcript,
        format!("{}\n", json!({"cwd": root.to_string_lossy()})),
    )
    .unwrap();

    let output = run_null(&fixture, &row, &["--print-command"], &fixture.bins);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(root.to_str().unwrap()), "{stdout}");
    assert!(!stdout.contains(stale_cwd.to_str().unwrap()), "{stdout}");
    assert!(stdout.contains("exec env FNO_AGENT_SELF="), "{stdout}");
    assert!(!stdout.contains("fake-executed"), "{stdout}");
    assert!(fixture.events().is_empty(), "{}", fixture.events());
}

#[test]
fn routed_codex_terminal_resume_composes_pre_exec_after_route_args() {
    let fixture = Fixture::new();
    std::os::unix::fs::symlink("/bin/sh", fixture.bins.join("sh")).unwrap();
    let row = fixture.row("codex", "pane", false).clone();
    let mut entries = fixture
        .rows
        .iter()
        .map(|row| row_json(row, fixture.root.path()))
        .collect::<Vec<_>>();
    let entry = entries
        .iter_mut()
        .find(|entry| entry["name"] == row.name)
        .unwrap();
    entry["route_provider_id"] = json!("zai-openai");
    entry["model_name"] = json!("glm-route-test");
    fixture.write_registry_entries(&entries);

    let config_dir = fixture.root.path().join(".fno");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        "[model_routing.providers.zai-openai]\nprotocol = \"openai\"\nbase_url = \"https://example.invalid/v1\"\napi_key_env = \"FNO_TEST_ZAI_KEY\"\n",
    )
    .unwrap();

    let (code, output) = run_terminal_with_env(
        &fixture,
        &row,
        &fixture.bins,
        &[("FNO_TEST_ZAI_KEY", "must-not-print-this-key")],
    );

    assert_eq!(code, 0, "{output}");
    assert!(output.contains("fake-executed"), "{output}");
    assert!(output.contains("model_providers.zai-openai"), "{output}");
    assert!(!output.contains("must-not-print-this-key"), "{output}");
}
