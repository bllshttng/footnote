//! Codex arm tests. The byte-parity target is the committed golden; the bind
//! tests drive [`BindProbe`]'s injected oracles, never a real daemon.
use super::*;
use std::process::Output;

/// The committed Python-leg capture; committed beside the spine (PR 1).
const GOLDEN: &str = include_str!("../../tests/golden/pane_spawn/codex_python_leg.json");

fn out(code: i32, stdout: &str, stderr: &str) -> Output {
    use std::os::unix::process::ExitStatusExt;
    Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

/// A canned transport: `pane ls` answers from the listing fields, `pane
/// read` (the tail read) from `read_stdout`, everything else exits 0.
struct FakeMux {
    ls_ok: bool,
    ls_stdout: String,
    read_stdout: String,
}

impl Default for FakeMux {
    fn default() -> Self {
        FakeMux {
            ls_ok: true,
            ls_stdout: "[]".to_string(),
            read_stdout: String::new(),
        }
    }
}

impl MuxCli for FakeMux {
    fn run(&self, args: &[String], _env: &[(String, String)]) -> std::io::Result<Output> {
        if args.len() > 2 && args[2] == "ls" {
            let code = if self.ls_ok { 0 } else { 1 };
            return Ok(out(code, &self.ls_stdout, ""));
        }
        Ok(out(0, &self.read_stdout, ""))
    }
}

fn ls_rows(rows: &[Value]) -> String {
    serde_json::to_string(rows).unwrap()
}

fn listing_with_pane(pane_id: u64) -> FakeMux {
    FakeMux {
        ls_stdout: ls_rows(&[serde_json::json!({"pane_id": pane_id, "cwd": "/repo"})]),
        ..Default::default()
    }
}

fn listing_without_pane() -> FakeMux {
    FakeMux {
        ls_stdout: ls_rows(&[serde_json::json!({"pane_id": 999, "cwd": "/repo"})]),
        ..Default::default()
    }
}

fn pane_facts(child_pid: Option<u32>) -> PaneFacts {
    PaneFacts {
        session: "main".to_string(),
        pane_id: 7,
        child_pid,
        spawn_started_ms: now_ms(),
    }
}

/// The golden tokens from the first `codex` token onward.
fn golden_codex_tail(case: &str) -> Vec<String> {
    let golden: Value = serde_json::from_str(GOLDEN).unwrap();
    let argv = golden["cases"][case]["pane_argv"]
        .as_array()
        .expect("pane_argv array")
        .iter()
        .map(|token| token.as_str().expect("string token").to_string())
        .collect::<Vec<_>>();
    let start = argv
        .iter()
        .position(|token| token == "codex")
        .expect("codex head");
    argv[start..].to_vec()
}

/// Set FNO_CODEX_VERSION for one test body and restore whatever was there,
/// so hook_trust_flag's version gate answers deterministically under
/// cfg(test) (which otherwise emits no flag). The lock serializes every
/// pinner: the env var is process-global, and one test's restore must never
/// delete another test's pin mid-read (the flaky parity failure the first
/// parallel CI run hit).
static CODEX_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_pinned_codex_version(body: impl FnOnce()) {
    let _guard = CODEX_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let key = "FNO_CODEX_VERSION";
    let saved = std::env::var(key).ok();
    std::env::set_var(key, "0.156.0");
    body();
    match saved {
        Some(value) => std::env::set_var(key, value),
        None => std::env::remove_var(key),
    }
}

#[test]
fn wrapped_argv_matches_golden_argv_plain() {
    with_pinned_codex_version(|| {
        let mut ctx = PaneContext::simple("codex", "/repo");
        ctx.message = "do x".to_string();
        let plan = build_wrapped(&ctx);
        let mut expected = golden_codex_tail("argv_plain");
        // The golden captured build_pane_argv BEFORE the seed strip; the
        // arm's wrapped is the argv with the seed HELD for turn/start, so
        // the fence tokens are the parity delta.
        assert_eq!(
            expected.split_off(expected.len() - 2),
            vec!["--".to_string(), "do x".to_string()]
        );
        assert_eq!(plan.wrapped, expected);
        assert_eq!(plan.held_seed.as_deref(), Some("do x"));
        assert_eq!(plan.preassigned_id, None);
    });
}

#[test]
fn wrapped_argv_matches_golden_seed_leading_flag() {
    with_pinned_codex_version(|| {
        let mut ctx = PaneContext::simple("codex", "/repo");
        ctx.message = "--model opus do x".to_string();
        let plan = build_wrapped(&ctx);
        let mut expected = golden_codex_tail("argv_seed_leading_flag");
        // A leading-flag seed is fenced PROMPT, never parsed as a flag: the
        // arm holds the whole seed for turn/start either way.
        assert_eq!(
            expected.split_off(expected.len() - 2),
            vec!["--".to_string(), "--model opus do x".to_string()]
        );
        assert_eq!(plan.wrapped, expected);
        assert_eq!(plan.held_seed.as_deref(), Some("--model opus do x"));
    });
}

#[test]
fn git_grant_refusal_exact_line_and_exit() {
    let root = std::env::temp_dir().join(format!("fno-codex-pane-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut ctx = PaneContext::simple("codex", root.to_str().unwrap());
    ctx.message = "/target do the work".to_string();
    let refusal = refuse_without_git_grant(&ctx).unwrap_err();
    assert_eq!(refusal.exit_code, EXIT_REFUSAL);
    assert_eq!(
        refusal.line,
        format!(
            "fno agents spawn: Codex code payloads require a resolved git grant; cwd={}; \
             git rev-parse --git-common-dir failed; no worker launched",
            root.display()
        )
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn non_code_payload_skips_git_grant_refusal() {
    let root = std::env::temp_dir().join(format!("fno-codex-pane-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut ctx = PaneContext::simple("codex", root.to_str().unwrap());
    ctx.message = "do x".to_string();
    assert!(refuse_without_git_grant(&ctx).is_ok());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn sole_new_id_one_binds_zero_and_two_decline() {
    let mut loaded: HashSet<String> = HashSet::new();
    loaded.insert("a".to_string());
    let mut baseline: HashSet<String> = HashSet::new();
    baseline.insert("a".to_string());
    assert_eq!(sole_new_id(&loaded, &baseline), None);
    loaded.insert("b".to_string());
    assert_eq!(sole_new_id(&loaded, &baseline), Some("b".to_string()));
    loaded.insert("c".to_string());
    assert_eq!(sole_new_id(&loaded, &baseline), None);
}

#[test]
fn passthrough_collision_refuses_and_value_tokens_pass() {
    let emitted = vec![
        "-c".to_string(),
        "shell_environment_policy.set.FNO_AGENT_SELF=\"w\"".to_string(),
        "--remote".to_string(),
    ];
    let clash = vec!["--remote".to_string()];
    assert!(refuse_passthrough_collision(&clash, &emitted).is_err());
    let joined_flag = vec!["--remote=tight".to_string()];
    assert!(refuse_passthrough_collision(&joined_flag, &emitted).is_err());
    let fence = vec!["--".to_string(), "do x".to_string()];
    assert!(refuse_passthrough_collision(&fence, &emitted).is_ok());
    assert!(refuse_passthrough_collision(&[], &emitted).is_ok());
}

#[test]
fn identity_leaf_quotes_like_json_dumps() {
    assert!(identity_leaf(None).is_empty());
    assert!(identity_leaf(Some("")).is_empty());
    let leaf = identity_leaf(Some("w-plain"));
    assert_eq!(
        leaf,
        vec![
            "-c".to_string(),
            "shell_environment_policy.set.FNO_AGENT_SELF=\"w-plain\"".to_string(),
        ]
    );
    assert!(shell_env_leaf("9BAD", "v").is_none());
}

#[test]
fn probe_binds_on_fd_answer_without_daemon() {
    let baseline = HashSet::new();
    let mut probe = BindProbe {
        baseline: Some(&baseline),
        fd_probe: Box::new(|| Some("s-fd".to_string())),
        daemon_probe: Box::new(|| panic!("daemon oracle consulted before the fd leg")),
        alive: Box::new(|| Some(true)),
        now_ms_fn: Box::new(now_ms),
        last_daemon_probe_ms: 0,
        prev_candidate: None,
    };
    assert_eq!(probe.probe(), Some("s-fd".to_string()));
}

#[test]
fn daemon_candidate_needs_two_consecutive_probes_then_liveness() {
    let baseline = HashSet::new();
    let tick = std::cell::Cell::new(0i64);
    let mut probe = BindProbe {
        baseline: Some(&baseline),
        fd_probe: Box::new(|| None),
        daemon_probe: Box::new(|| Some("s9".to_string())),
        alive: Box::new(|| Some(true)),
        now_ms_fn: Box::new(move || {
            tick.set(tick.get() + 3_000);
            tick.get()
        }),
        last_daemon_probe_ms: 0,
        prev_candidate: None,
    };
    // First answer: held for a repeat, never trusted alone.
    assert_eq!(probe.probe(), None);
    // Second answer past the rate limit, pane alive: trusted.
    assert_eq!(probe.probe(), Some("s9".to_string()));
}

#[test]
fn daemon_candidate_blocked_by_confirmed_pane_death() {
    let baseline = HashSet::new();
    let tick = std::cell::Cell::new(0i64);
    let mut probe = BindProbe {
        baseline: Some(&baseline),
        fd_probe: Box::new(|| None),
        daemon_probe: Box::new(|| Some("s9".to_string())),
        alive: Box::new(|| Some(false)),
        now_ms_fn: Box::new(move || {
            tick.set(tick.get() + 3_000);
            tick.get()
        }),
        last_daemon_probe_ms: 0,
        prev_candidate: None,
    };
    assert_eq!(probe.probe(), None);
    assert_eq!(probe.probe(), None);
}

#[test]
fn window_expiry_reports_the_clock_reason() {
    let baseline = HashSet::new();
    let mut probe = BindProbe {
        baseline: Some(&baseline),
        fd_probe: Box::new(|| None),
        daemon_probe: Box::new(|| None),
        alive: Box::new(|| Some(true)),
        now_ms_fn: Box::new(now_ms),
        last_daemon_probe_ms: 0,
        prev_candidate: None,
    };
    let mux = listing_with_pane(7);
    let pane = pane_facts(None);
    let bind = await_binding(Duration::ZERO, &mux, &pane, &mut probe);
    assert!(matches!(
        bind,
        ArmBind::Unbound { reason } if reason == "binding-window-expired"
    ));
}

#[test]
fn window_expiry_names_the_disabled_oracle() {
    let mut probe = BindProbe {
        baseline: None,
        fd_probe: Box::new(|| None),
        daemon_probe: Box::new(|| panic!("disabled oracle consulted")),
        alive: Box::new(|| Some(true)),
        now_ms_fn: Box::new(now_ms),
        last_daemon_probe_ms: 0,
        prev_candidate: None,
    };
    let mux = listing_with_pane(7);
    let pane = pane_facts(None);
    let bind = await_binding(Duration::ZERO, &mux, &pane, &mut probe);
    assert!(matches!(
        bind,
        ArmBind::Unbound { reason }
            if reason == "daemon: no pre-spawn baseline, oracle disabled for this spawn"
    ));
}

#[test]
fn confirmed_pane_death_returns_the_death_reason() {
    let baseline = HashSet::new();
    let mut probe = BindProbe {
        baseline: Some(&baseline),
        fd_probe: Box::new(|| None),
        daemon_probe: Box::new(|| None),
        alive: Box::new(|| Some(true)),
        now_ms_fn: Box::new(now_ms),
        last_daemon_probe_ms: 0,
        prev_candidate: None,
    };
    // A NON-EMPTY listing without our pane is the only confirmed death.
    let mux = listing_without_pane();
    let pane = pane_facts(None);
    let bind = await_binding(Duration::from_secs(60), &mux, &pane, &mut probe);
    assert!(matches!(
        bind,
        ArmBind::Died { reason } if reason == "pane-died-before-binding"
    ));
}

#[test]
fn seed_failure_keeps_the_pane() {
    let pane = pane_facts(Some(4242));
    let refusal = seed_refusal("daemon refused the turn", &pane);
    assert_eq!(refusal.exit_code, EXIT_SPAWN_FAILED);
    assert!(refusal.after_pane.is_none());
    assert_eq!(
        refusal.line,
        "agent seed never submitted after bind (daemon refused the turn); pane 7 kept"
    );
}

#[test]
fn late_bind_reprobes_the_fd_oracle_within_budget() {
    let dir = std::env::temp_dir().join(format!("fno-codex-late-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rollout-test-late.jsonl");
    std::fs::write(&path, r#"{"payload":{"id":"late-sess"}}"#).unwrap();
    let held = std::fs::File::open(&path).unwrap();
    let sid = late_bind_probe(
        Duration::from_millis(500),
        Duration::from_millis(10),
        Some(std::process::id()),
    );
    assert_eq!(sid, Some("late-sess".to_string()));
    drop(held);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn late_bind_returns_none_on_a_miss() {
    // pid 1's fd table (or a permission-denied read of it) never holds a
    // fno rollout, so the miss is deterministic and parallel-safe.
    let sid = late_bind_probe(Duration::ZERO, Duration::ZERO, Some(1));
    assert_eq!(sid, None);
}

#[test]
fn identity_leaf_splices_after_the_codex_token_when_named() {
    with_pinned_codex_version(|| {
        let mut ctx = PaneContext::simple("codex", "/repo");
        ctx.message = "do x".to_string();
        ctx.worker_name = "w-plain".to_string();
        let plan = build_wrapped(&ctx);
        // Expected: the golden argv_plain tail minus the seed fence, with
        // the FNO_AGENT_SELF leaf spliced after the codex token - the
        // shape the full-leg goldens pin (plain_pane.pane_run_argv).
        let mut expected = golden_codex_tail("argv_plain");
        let fence = expected.split_off(expected.len() - 2);
        assert_eq!(fence, vec!["--".to_string(), "do x".to_string()]);
        expected.splice(1..1, identity_leaf(Some("w-plain")));
        assert_eq!(plan.wrapped, expected);
    });
}

#[test]
fn passthrough_appends_then_add_dirs_strip() {
    with_pinned_codex_version(|| {
        let mut ctx = PaneContext::simple("codex", "/repo");
        ctx.passthrough = vec![
            "--model".to_string(),
            "opus".to_string(),
            "--add-dir".to_string(),
            "/x".to_string(),
            "--add-dir=/y".to_string(),
        ];
        let plan = build_wrapped(&ctx);
        // The grants die for a --remote argv; the flags survive at the tail.
        let starts_at = plan
            .wrapped
            .iter()
            .position(|token| token == "--model")
            .expect("passthrough rides at the tail");
        assert_eq!(
            &plan.wrapped[starts_at..],
            &["--model".to_string(), "opus".to_string()]
        );
        assert!(
            !plan
                .wrapped
                .iter()
                .any(|token| token.starts_with("--add-dir")),
            "no --add-dir survives the strip"
        );
    });
}

#[test]
fn node_dispatch_is_a_code_payload_without_a_message_prefix() {
    let root = std::env::temp_dir().join(format!("fno-codex-node-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut ctx = PaneContext::simple("codex", root.to_str().unwrap());
    ctx.message = "do x".to_string();
    ctx.node = Some("fno-abcd".to_string());
    let refusal = refuse_without_git_grant(&ctx).unwrap_err();
    assert_eq!(refusal.exit_code, EXIT_REFUSAL);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_launch_cwd_is_pre_trusted_in_the_codex_config() {
    let _guard = CODEX_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = tempfile::tempdir().unwrap();
    let saved = std::env::var_os("CODEX_HOME");
    std::env::set_var("CODEX_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    // A fresh never-trusted dir: the upsert writes the exact key codex reads.
    assert!(super::ensure_codex_folder_trust(project.path()));
    let text = std::fs::read_to_string(home.path().join("config.toml")).expect("config written");
    assert!(
        text.contains(project.path().to_string_lossy().as_ref())
            && text.contains("trust_level = \"trusted\""),
        "{text}"
    );
    // A second call answers true without needing a rewrite.
    assert!(super::ensure_codex_folder_trust(project.path()));
    // A malformed existing config is never clobbered.
    std::fs::write(home.path().join("config.toml"), "not toml [ at all").unwrap();
    assert!(!super::ensure_codex_folder_trust(project.path()));
    assert_eq!(
        std::fs::read_to_string(home.path().join("config.toml")).unwrap(),
        "not toml [ at all"
    );
    // A `projects` key that is not a table is refused untouched too.
    std::fs::write(home.path().join("config.toml"), "projects = \"nope\"\n").unwrap();
    assert!(!super::ensure_codex_folder_trust(project.path()));
    assert_eq!(
        std::fs::read_to_string(home.path().join("config.toml")).unwrap(),
        "projects = \"nope\"\n"
    );
    match saved {
        Some(v) => std::env::set_var("CODEX_HOME", v),
        None => std::env::remove_var("CODEX_HOME"),
    }
}
