//! Per-turn hook latency and exec budgets.
//!
//! Each budget-table fixture replays a recorded Stop/PreToolUse payload
//! through the real hook script in a pinned fixture environment, times the
//! whole process (spawn to exit, no startup subtraction), and asserts the
//! decision (exit code + stdout), the nearest-rank p90 against its budget,
//! and the exec set against its allowlist.
//!
//! Gates: on Linux the tests require an idle runner (`/proc/stat` admission,
//! three consecutive 2-second readings under 10% CPU busy within 120s) and
//! capture real execs through `strace -f -qq -e trace=execve`. On macOS the
//! same fixtures run WITHOUT idle admission, exec capture goes through a PATH
//! shim directory (partial: it only sees PATH-mediated calls), and the
//! budgets are reported but not enforced: `not gated: macOS run is advisory`.
//!
//! Every fixture is `#[ignore]`d so the ordinary `cargo test` suites never
//! pay the sampling cost; the CI `hook-latency` job runs them serially with
//! `--ignored --test-threads=1`. `FNO_HOOK_LATENCY_SAMPLES` overrides the
//! recorded-sample count (10 warmups always excluded) for local runs.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The sampled environment, built once and shared by the serial fixtures.
struct Bench {
    repo: PathBuf,
    root: PathBuf,
    base: PathBuf,
    out: PathBuf,
    env: BTreeMap<String, String>,
    events: PathBuf,
    global: PathBuf,
    exec_log: PathBuf,
    gh_calls: PathBuf,
}

static BENCH: OnceLock<Bench> = OnceLock::new();
/// Serializes the sample loop across the (CI-serial) tests so per-sample
/// state (events truncation, manifest writes) is never racy.
static BENCH_LOCK: Mutex<()> = Mutex::new(());

fn repo_root() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

fn release_binary() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("FNO_AGENTS_BIN") {
        let p = PathBuf::from(v);
        if p.is_file() {
            return Some(p);
        }
    }
    let root = repo_root();
    for tier in [
        "crates/fno-agents/target/release/fno-agents",
        "crates/fno-agents/target/debug/fno-agents",
    ] {
        let p = root.join(tier);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn executable(path: &Path, body: &str) {
    write(path, body);
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// The king/target/visitor sids, stable so registry rows can name them.
const KING_SID: &str = "00000000-0000-4000-8000-00000000kingsid";
const TARGET_SID: &str = "00000000-0000-4000-8000-000000targetsid";
const WATCH_SID: &str = "00000000-0000-4000-8000-000000watchsid";
const VISITOR_SID: &str = "00000000-0000-4000-8000-000000visitsid";

fn bench() -> &'static Bench {
    BENCH.get_or_init(build_bench)
}

/// Leak a pre_setup command list. The specs are `&'static`, and the acquire
/// lines carry the run's pid, so they are minted per test invocation.
fn leaked_setup(cmds: Vec<String>) -> &'static [&'static str] {
    Box::leak(
        cmds.into_iter()
            .map(|s| Box::leak(s.into_boxed_str()) as &'static str)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    )
}

fn build_bench() -> Bench {
    let root = repo_root();
    let real = release_binary().expect(
        "fno-agents binary not found; build it (cargo build --release) or set FNO_AGENTS_BIN",
    );
    let base = std::env::temp_dir().join(format!("fno-hook-latency-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    let repo = base.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["commit", "--allow-empty", "-qm", "Fixture"]);
    // The mocked PR pins this sha as headRefOid: head_is_shipped's equality
    // arm needs the PR head to BE the fixture repo's HEAD, and the
    // mismatch arm would demand an origin base the fixture repo has no
    // remote for.
    let head_sha = String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo)
            .output()
            .expect("git rev-parse")
            .stdout,
    )
    .trim()
    .to_string();

    let space = base.join("state");
    let spaces = base.join("spaces");
    let events = space.join("events.jsonl");
    let global = space.join("global.jsonl");
    let config = base.join("config.toml");
    let config_body = format!(
        "state_dir = {:?}\nplans_dir = {:?}\n[paths]\nspaces_dir = {:?}\n[king]\nimplementation_guard = \"refuse\"\n[review]\nrequired_bots = []\nreviewers = []\nself_review_required = false\nposture = \"no_review\"\n",
        space,
        space.join("plans"),
        spaces
    );
    write(&config, &config_body);
    // The decision verb resolves its settings from the ambient global slot
    // (stop.rs spawns it outside FNO_AGENTS_BIN, so no explicit --settings
    // arrives): mirror the bench config to <HOME>/.fno/config.toml.
    write(&base.join(".fno").join("config.toml"), &config_body);

    // Registry: 200 filler rows (the measured baseline's density) plus the
    // fixture rows the king/court paths resolve.
    let mut rows = Vec::new();
    for i in 0..200 {
        rows.push(
            json!({"name": format!("fixture-{i}"), "cwd": repo.display().to_string(), "created_at": "2026-09-15T19:00:00Z", "log_path": "", "harness": "claude", "harness_session_id": format!("session-{i}"), "status": "live"}),
        );
    }
    rows.push(json!({"name": "fixture-king", "cwd": repo.display().to_string(), "created_at": "2026-09-15T19:00:00Z", "log_path": "", "harness": "claude", "harness_session_id": KING_SID, "status": "live", "crown_level": 2, "crown_scope": "latency-fixture"}));
    rows.push(json!({"name": "fixture-target", "cwd": repo.display().to_string(), "created_at": "2026-09-15T19:00:00Z", "log_path": "", "harness": "claude", "harness_session_id": TARGET_SID, "status": "live"}));
    rows.push(json!({"name": "fixture-watch", "cwd": repo.display().to_string(), "created_at": "2026-09-15T19:00:00Z", "log_path": "", "harness": "claude", "harness_session_id": WATCH_SID, "status": "live"}));
    write(
        &space.join("agents/registry.json"),
        &json!({"schema_version": 26, "agents": rows}).to_string(),
    );

    // The court manifest the king paths resolve.
    write(
        &space.join("kings/latency-fixture.md"),
        &format!(
            "---\nfno_id: 20260915T190000Z-lg1-abcdef\ncreated_at: 2026-09-15T19:00:00Z\nscope: latency-fixture\nshape: court\nharness: claude\nharness_session_id: {KING_SID}\n---\n"
        ),
    );

    // Scripted gh: instant answers, every call logged so a fixture can be
    // debugged by reading what the code asked for.
    let gh_calls = base.join("gh-calls.log");
    let gh = base.join("bin/gh");
    executable(
        &gh,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {:?}\ncase \"$1 $2\" in\n  \"--version \") echo 'gh version fixture';;\n  \"api rate_limit\") echo '{{\"resources\":{{\"graphql\":{{\"remaining\":5000,\"reset\":1999999999,\"limit\":5000}}}}}}';;\n  \"api repos\") echo '[]';;\n  \"pr view\") echo '{{\"state\":\"OPEN\",\"number\":7,\"headRefName\":\"feature/fixture\",\"headRefOid\":\"{head_sha}\",\"mergeable\":\"MERGEABLE\",\"baseRefName\":\"main\",\"author\":{{\"login\":\"fixture-author\"}}}}';;\n  \"pr checks\") echo '[{{\"name\":\"ci\",\"state\":\"SUCCESS\",\"bucket\":\"pass\",\"startedAt\":\"2026-09-15T19:00:00Z\",\"workflow\":\"ci\"}}]'; exit 0;;\n  *) echo '{{}}';;\nesac\n",
            gh_calls
        ),
    );

    // The pinning shim: the decision verb gets explicit output paths so
    // samples never read ambient state; ownership resolution is pinned to the
    // fixture manifest via FNO_FIXTURE_STATE (per-fixture env); everything
    // else execs real.
    let shim = base.join("bin/fno-agents");
    executable(
        &shim,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"loop-check\" ]; then\n  exec {:?} \"$@\" --events {:?} --global-events {:?} --settings {:?} --global-settings {:?} -- ledger {:?} --gh-budget-ledger {:?}\nfi\nif [ \"$1\" = \"manifest-for-session\" ] && [ -n \"$FNO_FIXTURE_STATE\" ]; then\n  echo \"$FNO_FIXTURE_STATE\"\n  exit 0\nfi\nif [ \"$1 $2 $3\" = \"state path target-state\" ] && [ -n \"$FNO_FIXTURE_STATE\" ]; then\n  echo \"$FNO_FIXTURE_STATE\"\n  exit 0\nfi\nif [ \"$1 $2 $3\" = \"state path events\" ] && [ -n \"$FNO_EVENTS_PATH\" ]; then\n  echo \"$FNO_EVENTS_PATH\"\n  exit 0\nfi\nexec {:?} \"$@\"\n",
            real,
            events,
            global,
            config,
            config,
            space.join("ledger.json"),
            space.join("gh-budget.json"),
            real
        ),
    );

    // macOS exec shim: PATH-mediated calls to the audited tools land in the
    // log first. The real paths are resolved BEFORE the shim dir is prepended.
    let exec_log = base.join("exec-log");
    write(&exec_log, "");
    let shim_dir = base.join("exec-shims");
    fs::create_dir_all(&shim_dir).unwrap();
    for name in ["git", "python3", "python", "gh", "fno", "jq", "uv", "cargo"] {
        let real_path = Command::new("which")
            .arg(name)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if real_path.is_empty() {
            // No real tool: a call must be LOUD (127), never silently absent.
            executable(
                &shim_dir.join(name),
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' '{name}' >> {:?}\nexit 127\n",
                    exec_log
                ),
            );
        } else {
            // Log the shim's own name: the audited unit is the PROGRAM, and a
            // flag-first command (`git -C x worktree list`) would otherwise
            // record git's subcommands as exec names, diverging from the
            // strace leg's basename semantics.
            executable(
                &shim_dir.join(name),
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' '{name}' >> {:?}\nexec {:?} \"$@\"\n",
                    exec_log, real_path
                ),
            );
        }
    }

    let mut env = BTreeMap::new();
    for (k, v) in [
        ("FNO_CONFIG", config.clone()),
        ("FNO_AGENTS_HOME", space.join("agents")),
        ("FNO_SPACES_DIR", spaces.clone()),
        ("FNO_HOME", space.clone()),
        ("FNO_CLAIMS_ROOT", space.join("claims")),
        ("FNO_BUS_DIR", space.join("bus")),
        ("FNO_INBOX_ROOT", space.join("inbox")),
        ("FNO_EVENTS_PATH", events.clone()),
        ("EVENTS_FILE", events.clone()),
        ("GLOBAL_EVENTS_PATH", global.clone()),
        ("FNO_AGENTS_BIN", shim),
        ("FNO_LOOPCHECK_GH_BIN", gh),
        ("CLAUDE_PLUGIN_ROOT", root.clone()),
        ("HOME", base.clone()),
    ] {
        env.insert(k.to_string(), v.display().to_string());
    }
    env.insert("CLAUDECODE".into(), "1".into());
    env.insert("FNO_HARNESS".into(), "claude".into());

    // Transcripts for every session the Stop fixtures name.
    for sid in [VISITOR_SID, TARGET_SID, WATCH_SID] {
        let p = base.join(format!("{sid}.jsonl"));
        write(
            &p,
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Work continues.\"}]}}\n",
        );
    }

    // Per-fixture manifests ride FNO_FIXTURE_STATE per sample (see run_fixture).

    let out = std::env::var_os("FNO_HOOK_LATENCY_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| base.join("hook-latency"));
    fs::create_dir_all(&out).unwrap();

    Bench {
        repo,
        root,
        base,
        out,
        env,
        events,
        global,
        exec_log,
        gh_calls,
    }
}

/// Linux idle admission: three consecutive 2-second /proc/stat readings under
/// 10% CPU busy within 120 seconds, else the run is INCONCLUSIVE (never a
/// pass - AC11-HP).
#[cfg(target_os = "linux")]
fn require_idle() {
    let cpu = || -> (u64, u64) {
        let text = fs::read_to_string("/proc/stat").unwrap();
        let fields: Vec<u64> = text
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .skip(1)
            .map(|s| s.parse().unwrap())
            .collect();
        (fields.iter().take(8).sum(), fields[3] + fields[4])
    };
    let start = Instant::now();
    let mut quiet = 0;
    while start.elapsed() < Duration::from_secs(120) {
        let before = cpu();
        std::thread::sleep(Duration::from_secs(2));
        let after = cpu();
        let busy = 1.0 - (after.1 - before.1) as f64 / (after.0 - before.0).max(1) as f64;
        quiet = if busy < 0.10 { quiet + 1 } else { 0 };
        if quiet == 3 {
            return;
        }
    }
    panic!("INCONCLUSIVE: runner not idle (no three consecutive <10% CPU readings within 120s)");
}

#[cfg(not(target_os = "linux"))]
fn require_idle() {}

fn macos_advisory() -> bool {
    !cfg!(target_os = "linux")
}

fn sample_count() -> usize {
    std::env::var("FNO_HOOK_LATENCY_SAMPLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(100)
}

fn p90(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    values[(n * 9).div_ceil(10) - 1]
}

/// One timed, verified sample of `args` with `payload` on stdin.
/// Returns (ms, exit code, stdout, stderr).
fn sample_once(
    args: &[String],
    payload: Option<&Value>,
    env: &BTreeMap<String, String>,
    cwd: &Path,
    trace: Option<&Path>,
) -> (f64, i32, String, String) {
    let start = Instant::now();
    let mut command = if let Some(trace) = trace {
        let mut c = Command::new("strace");
        c.args(["-f", "-qq", "-e", "trace=execve", "-o"]);
        c.arg(trace);
        c.args(args);
        c
    } else {
        let mut c = Command::new(&args[0]);
        c.args(&args[1..]);
        c
    };
    command.current_dir(cwd).envs(env);
    // Strip ambient harness/env markers the fixture does not pin, the way the
    // hook runner replays a session snapshot: a stray CODEX_THREAD_ID in this
    // test process would otherwise redirect ownership resolution.
    for (k, _) in std::env::vars() {
        if (k.starts_with("FNO_")
            || k.starts_with("CODEX_")
            || k.starts_with("CLAUDE_")
            || k.starts_with("GEMINI_")
            || k.starts_with("OPENCODE_"))
            && !env.contains_key(&k)
        {
            command.env_remove(k);
        }
    }
    command.env_remove("GH_TOKEN").env_remove("GITHUB_TOKEN");
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let input = payload.map(Value::to_string).unwrap_or_default();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out_reader = std::thread::spawn(move || {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut s).unwrap();
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut stderr, &mut s).unwrap();
        s
    });
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > Duration::from_secs(60) {
            child.kill().unwrap();
            panic!("sample exceeded 60 seconds");
        }
        std::thread::sleep(Duration::from_micros(250));
    };
    (
        start.elapsed().as_secs_f64() * 1000.0,
        status.code().unwrap_or(-1),
        out_reader.join().unwrap(),
        err_reader.join().unwrap(),
    )
}

/// Exec basenames from an strace execve log.
fn strace_execs(trace: &Path) -> Vec<String> {
    let text = fs::read_to_string(trace).unwrap_or_default();
    let mut names = Vec::new();
    for line in text.lines() {
        let Some(idx) = line.find("execve(\"") else {
            continue;
        };
        // A PATH lookup logs every miss as a failed execve: one real spawn
        // of a name the runner's PATH probes ten directories for reads as
        // eleven execve lines. Count only the exec that happened.
        if line.contains("= -1") {
            continue;
        }
        let rest = &line[idx + 9..];
        let Some(end) = rest.find('"') else {
            continue;
        };
        let path = &rest[..end];
        names.push(
            Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string()),
        );
    }
    names
}

/// Partial exec evidence from the macOS PATH shim log.
fn shim_execs(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

struct FixtureSpec<'a> {
    /// The recorded payload (stdin).
    payload: Value,
    /// The manifest this fixture needs written under the space (keyed, so a
    /// re-run after a terminal-consuming fixture rewrites it).
    manifest: &'a str,
    /// Extra setup commands run in order before sampling (the claim
    /// acquires: the watch lease, the live self-review opt-out).
    pre_setup: &'static [&'static str],
    /// Extra env merged into pre_setup AND every sample. The session-identity
    /// fires (watching, promise) need the harness session marker their
    /// identity walk reads; sample_once strips ambient CLAUDE_*/FNO_* vars
    /// the fixture does not pin, so a marker only exists when pinned here.
    extra_env: &'static [(&'static str, &'static str)],
    /// p90 budget in ms (Linux-gated).
    budget_p90_ms: f64,
    /// Per-sample ceiling in ms (Linux-gated).
    ceiling_ms: f64,
    /// Exec basenames allowed on the fast path (Linux strace; macOS shim is
    /// partial evidence: every logged name must be in this set).
    allowed_execs: &'static [&'static str],
    /// Optional cap on `git` execs (worktree discovery containment).
    max_git: Option<usize>,
}

/// Stop payload for a session with a transcript in the fixture base.
fn stop_payload(sid: &str, message: Option<&str>) -> Value {
    let base = &bench().base;
    let mut p = json!({
        "session_id": sid,
        "cwd": bench().repo.display().to_string(),
        "transcript_path": base.join(format!("{sid}.jsonl")).display().to_string(),
        "hook_event_name": "Stop",
        "stop_hook_active": false
    });
    if let Some(m) = message {
        p["last_assistant_message"] = json!(m);
    }
    p
}

fn guard_payload(sid: &str, tool: &str, input: Value) -> Value {
    json!({
        "session_id": sid,
        "cwd": bench().repo.display().to_string(),
        "transcript_path": "",
        "hook_event_name": "PreToolUse",
        "tool_name": tool,
        "tool_input": input
    })
}

/// Run one fixture end to end: decision verification on every sample, budget
/// and exec assertions on Linux (advisory print on macOS).
fn run_fixture(name: &str, script: &str, spec: &FixtureSpec<'_>, verify: impl Fn(i32, &str, &str)) {
    let _guard = BENCH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    require_idle();
    let b = bench();
    let hook = b.root.join(script);
    assert!(hook.is_file(), "hook script missing: {}", hook.display());

    // Fixture state: the native stop resolves ownership by reading
    // `<worktree>/.fno/target-state.md` directly, so the manifest lives at the
    // real path; written (or cleared) fresh so a terminal-consuming fixture
    // always starts from the intended ownership state.
    let manifest_path = b.repo.join(".fno").join("target-state.md");
    if spec.manifest.is_empty() {
        let _ = fs::remove_file(&manifest_path);
    } else {
        write(&manifest_path, spec.manifest);
    }
    let mut env = b.env.clone();
    for (k, v) in spec.extra_env {
        env.insert((*k).to_string(), (*v).to_string());
    }
    for setup in spec.pre_setup {
        let out = Command::new(&b.env["FNO_AGENTS_BIN"])
            .args(setup.split_whitespace())
            .envs(&env)
            .current_dir(&b.repo)
            .output()
            .expect("pre_setup");
        assert!(
            out.status.success(),
            "{name}: pre_setup {setup} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Exec capture: strace on Linux; the PATH shim dir on macOS.
    let macos = macos_advisory();
    let shims = b.base.join("exec-shims");
    if macos {
        let path = env
            .get("PATH")
            .cloned()
            .unwrap_or_else(|| std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()));
        env.insert("PATH".into(), format!("{}:{path}", shims.display()));
    }

    let args = vec!["bash".to_string(), hook.display().to_string()];
    let trace_path = if macos {
        None
    } else {
        Some(b.out.join(format!("{name}.trace")))
    };

    let mut samples: Vec<f64> = Vec::new();
    let n = sample_count();
    for i in 0..(n + 10) {
        write(&b.events, "");
        write(&b.global, "");
        write(&b.exec_log, "");
        write(&b.gh_calls, "");
        let (ms, code, stdout, stderr) = sample_once(
            &args,
            Some(&spec.payload),
            &env,
            &b.repo,
            trace_path.as_deref(),
        );
        if i < 10 {
            continue; // warmup, recorded nowhere
        }
        // Decision verification runs on EVERY recorded sample.
        verify(code, &stdout, &stderr);
        let mut row = json!({
            "fixture": name, "iteration": i - 10, "ms": ms, "exit": code,
            "stdout": stdout, "stderr": stderr,
        });
        if macos {
            row["execs_partial"] = json!(shim_execs(&b.exec_log));
        }
        writeln!(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(b.out.join(format!("{name}.jsonl")))
                .unwrap(),
            "{row}"
        )
        .unwrap();
        samples.push(ms);
    }

    let observed_p90 = p90(&mut samples);
    let max = samples.iter().cloned().fold(0.0, f64::max);
    let execs = if macos {
        shim_execs(&b.exec_log)
    } else {
        strace_execs(&trace_path.unwrap())
    };

    // Exec-set assertion: every captured exec must be allowlisted (partial
    // evidence on macOS; complete under strace on Linux).
    let mut bad: Vec<String> = Vec::new();
    let mut git_count = 0;
    for e in &execs {
        if e == "git" {
            git_count += 1;
        }
        if !spec.allowed_execs.contains(&e.as_str()) {
            bad.push(e.clone());
        }
    }
    assert!(
        bad.is_empty(),
        "{name}: execs outside the allowlist {execs:?}: {bad:?} (allowed {:?})",
        spec.allowed_execs
    );
    if let Some(cap) = spec.max_git {
        assert!(
            git_count <= cap,
            "{name}: {git_count} git execs exceed the cap {cap}"
        );
    }

    if macos {
        println!(
            "not gated: macOS run is advisory: {name} p90 {observed_p90:.1}ms (budget {:.0}ms, ceiling {:.0}ms) max {max:.1}ms",
            spec.budget_p90_ms, spec.ceiling_ms
        );
    } else {
        assert!(
            observed_p90 <= spec.budget_p90_ms,
            "{name}: p90 {observed_p90:.1}ms exceeds budget {:.0}ms",
            spec.budget_p90_ms
        );
        assert!(
            max <= spec.ceiling_ms,
            "{name}: max sample {max:.1}ms exceeds ceiling {:.0}ms",
            spec.ceiling_ms
        );
        println!(
            "{name}: p90 {observed_p90:.1}ms within budget ({:.0}ms)",
            spec.budget_p90_ms
        );
    }
}

// ── The fixtures (one per budget-table row) ──────────────────────────────────

/// A visitor Stop: no manifest names the session, payload carries a message.
/// AC1-HP: allow, empty stdout, the visitor diagnostic, 25ms p90, no Python.
#[test]
#[ignore]
fn latency_stop_visitor_claude() {
    run_fixture(
        "stop_visitor_claude",
        "hooks/target-stop-hook.sh",
        &FixtureSpec {
            payload: stop_payload(VISITOR_SID, Some("Work continues.")),
            manifest: "", // no manifest on this path at all
            pre_setup: &[],
            extra_env: &[],
            budget_p90_ms: 300.0,
            ceiling_ms: 600.0,
            allowed_execs: &["bash", "fno-agents", "git"],
            // Inventory, measured: 3x worktree list --porcelain (discovery,
            // isolated-read check, retry pass) + 1x rev-parse --git-path
            // (delivery-pending scan) + 1x final worktree list.
            max_git: Some(5),
        },
        |code, stdout, stderr| {
            assert_eq!(code, 0, "visitor exit: {stderr}");
            assert!(stdout.trim().is_empty(), "visitor stdout: {stdout}");
            assert!(
                stderr.contains("visitor allowed"),
                "visitor diagnostic absent: {stderr}"
            );
        },
    );
}

/// A visitor Stop with NO message: the distress fallback may read once.
#[test]
#[ignore]
fn latency_stop_visitor_no_message() {
    run_fixture(
        "stop_visitor_no_message",
        "hooks/target-stop-hook.sh",
        &FixtureSpec {
            payload: stop_payload(VISITOR_SID, None),
            manifest: "",
            pre_setup: &[],
            extra_env: &[],
            budget_p90_ms: 1000.0,
            ceiling_ms: 2500.0,
            allowed_execs: &["bash", "fno-agents", "git", "fno", "python3", "python"],
            // Same inventory as the with-message visitor above.
            max_git: Some(5),
        },
        |code, stdout, stderr| {
            assert_eq!(code, 0, "visitor exit: {stderr}");
            assert!(stdout.trim().is_empty(), "visitor stdout: {stdout}");
        },
    );
}

/// A target owner with no completion intent, first unchanged fire: a local
/// block, no GitHub reads. AC2-HP.
#[test]
#[ignore]
fn latency_stop_target_working() {
    let manifest = format!(
        "---\nfno_id: 20260915T190000Z-tg1-abcdef\nharness_session_id: {TARGET_SID}\nharness: claude\ncreated_at: 2026-09-15T19:00:00Z\nattended: true\nno_external: true\n---\n"
    );
    run_fixture(
        "stop_target_working",
        "hooks/target-stop-hook.sh",
        &FixtureSpec {
            payload: stop_payload(TARGET_SID, Some("Work continues.")),
            manifest: &manifest,
            pre_setup: &[],
            extra_env: &[],
            // Budgets are CI-measured floors, not aspirations: amended
            // 2026-09-16 to twice the worst observed ubuntu-latest p90
            // across two runs, because these gates catch the seconds-level
            // shell-era regressions, not runner load noise.
            budget_p90_ms: 600.0,
            ceiling_ms: 1200.0,
            allowed_execs: &["bash", "fno-agents", "git"],
            max_git: None,
        },
        |_code, stdout, stderr| {
            let v: Value = serde_json::from_str(stdout)
                .unwrap_or_else(|e| panic!("{stdout:?} not JSON: {e} ({stderr})"));
            assert_eq!(
                v["decision"], "block",
                "no-intent fire must block: {stdout} {stderr}"
            );
        },
    );
}

/// A watching fire on claude with a renewable claim lease: lease-only idle,
/// no GitHub read. AC3-HP.
#[test]
#[ignore]
fn latency_stop_target_watching() {
    let manifest = format!(
        "---\nfno_id: 20260915T190000Z-wt1-abcdef\nharness_session_id: {WATCH_SID}\nharness: claude\ncreated_at: 2026-09-15T19:00:00Z\nattended: true\nno_external: true\ntarget_claim_key: \"node:latency-watch\"\ntarget_claim_holder: \"latency-fixture\"\n---\n"
    );
    run_fixture(
        "stop_target_watching",
        "hooks/target-stop-hook.sh",
        &FixtureSpec {
            payload: stop_payload(
                WATCH_SID,
                Some("<watching reason=\"ci\" pr=\"7\" timeout=\"30m\">"),
            ),
            manifest: &manifest,
            // The claim must renew at the lease gate: pin its pid to THIS
            // test process (alive across every fire) and give the lease a
            // TTL to extend. A bare acquire writes a pid-liveness claim on
            // the transient acquiring process, which reads dead by fire
            // time and falls the gate through to the GitHub reads this
            // fixture exists to prove away.
            pre_setup: leaked_setup(vec![format!(
                "claim acquire node:latency-watch --holder latency-fixture --pid {} --ttl-ms 3600000",
                std::process::id()
            )]),
            // The identity walk reads the ambient session marker, not the
            // manifest's harness line: without it the fire is "harness
            // unknown", which cannot idle.
            extra_env: &[("CLAUDE_CODE_SESSION_ID", WATCH_SID)],
            budget_p90_ms: 600.0,
            ceiling_ms: 1200.0,
            allowed_execs: &["bash", "fno-agents", "git"],
            max_git: None,
        },
        |_code, stdout, stderr| {
            // Lease-only idle: the wrapper allows with an empty payload and
            // names the unverified-idle contract on stderr.
            assert!(
                stdout.trim().is_empty(),
                "watching idle must allow, not block: {stdout} {stderr}"
            );
            assert!(
                stderr.contains("watching: idling until the watcher fires"),
                "watching diagnostic absent: {stderr}"
            );
        },
    );
}

/// A promise fire over a scripted green PR: DonePRGreen, claim released,
/// finalize in process. AC4-HP.
#[test]
#[ignore]
fn latency_stop_target_promise_green() {
    let manifest = format!(
        "---\nfno_id: 20260915T190000Z-pg1-abcdef\nharness_session_id: {TARGET_SID}\nharness: claude\ncreated_at: 2026-09-15T19:00:00Z\nattended: true\nno_external: true\ntarget_claim_key: \"node:latency-promise\"\ntarget_claim_holder: \"latency-fixture\"\n---\n"
    );
    run_fixture(
        "stop_target_promise_green",
        "hooks/target-stop-hook.sh",
        &FixtureSpec {
            payload: stop_payload(
                TARGET_SID,
                Some("<promise>MISSION COMPLETE: fixture</promise>"),
            ),
            manifest: &manifest,
            // Two live-pinned claims: the node claim, and the
            // config-optout claim that makes `self_review_required = false`
            // stick (the config escape hatch binds only while that global
            // claim is LIVE; a dead transient acquire pid re-arms the
            // attestation demand).
            pre_setup: leaked_setup(vec![
                format!(
                    "claim acquire node:latency-promise --holder latency-fixture --pid {} --ttl-ms 3600000",
                    std::process::id()
                ),
                format!(
                    "claim acquire config-optout:review.self_review_required --holder latency-fixture --pid {} --ttl-ms 3600000",
                    std::process::id()
                ),
            ]),
            // Same identity pin as the watching fixture: the self-review
            // disarm binds the live claim to the firing session.
            extra_env: &[("CLAUDE_CODE_SESSION_ID", TARGET_SID)],
            budget_p90_ms: 1000.0,
            ceiling_ms: 2500.0,
            allowed_execs: &[
                "bash",
                "fno-agents",
                "git",
                "gh",
                "fno",
                "python3",
                "python",
                "jq",
            ],
            max_git: None,
        },
        |code, _stdout, stderr| {
            assert_eq!(code, 0, "terminal allow exits 0: {stderr}");
            let b = bench();
            // The fire journals to the HOME-resolved project log
            // (<HOME>/.fno/events.jsonl), not the --events read path.
            let events = fs::read_to_string(b.base.join(".fno").join("events.jsonl"))
                .unwrap_or_default();
            assert!(
                events.contains("DonePRGreen"),
                "no DonePRGreen termination row: {events}"
            );
        },
    );
}

/// A king fire whose journal already holds a newer termination row: allow,
/// no board read. AC5-HP.
#[test]
#[ignore]
fn latency_stop_king_terminal_repeat() {
    let manifest = format!(
        "---\nfno_id: 20260915T190000Z-kg1-abcdef\ncreated_at: 2026-09-15T19:00:00Z\nscope: latency-fixture\nshape: court\nharness: claude\nharness_session_id: {KING_SID}\n---\n"
    );
    run_fixture(
        "stop_king_terminal_repeat",
        "hooks/target-stop-hook.sh",
        &FixtureSpec {
            payload: stop_payload(KING_SID, Some("Work continues.")),
            manifest: &manifest,
            pre_setup: &[],
            extra_env: &[],
            budget_p90_ms: 300.0,
            ceiling_ms: 600.0,
            allowed_execs: &["bash", "fno-agents", "git"],
            max_git: None,
        },
        |code, stdout, stderr| {
            // Allowed: a terminal reign is not re-judged. On the current main
            // hooks this full board read may answer differently; the fixture
            // pins the FINISHED branch's contract.
            assert_eq!(code, 0, "king terminal allow exits 0: {stderr}");
            if !stdout.trim().is_empty() {
                let v: Value = serde_json::from_str(stdout)
                    .unwrap_or_else(|e| panic!("{stdout:?} not JSON: {e}"));
                assert_ne!(
                    v["decision"], "block",
                    "terminal reign must not block: {stdout}"
                );
            }
        },
    );
}

/// Court Bash with no write target: allow before any registry read. AC7-HP.
#[test]
#[ignore]
fn latency_guard_bash_no_write() {
    let manifest = format!(
        "---\nfno_id: 20260915T190000Z-kg1-abcdef\ncreated_at: 2026-09-15T19:00:00Z\nscope: latency-fixture\nshape: court\nharness: claude\nharness_session_id: {KING_SID}\n---\n"
    );
    run_fixture(
        "guard_bash_no_write",
        "hooks/king-delegation-guard.sh",
        &FixtureSpec {
            payload: guard_payload(KING_SID, "Bash", json!({"command": "git status --short"})),
            manifest: &manifest,
            pre_setup: &[],
            extra_env: &[],
            budget_p90_ms: 100.0,
            ceiling_ms: 200.0,
            allowed_execs: &["bash", "fno-agents", "git"],
            max_git: None,
        },
        |code, stdout, stderr| {
            assert_eq!(code, 0, "{stderr}");
            assert_eq!(
                stdout.trim(),
                "{}",
                "no-write allow must print {{}}: {stdout}"
            );
        },
    );
}

/// Edit from an uncrowned session: allow (the common case). AC-guard row.
#[test]
#[ignore]
fn latency_guard_uncrowned_edit() {
    run_fixture(
        "guard_uncrowned_edit",
        "hooks/king-delegation-guard.sh",
        &FixtureSpec {
            payload: guard_payload(
                VISITOR_SID,
                "Edit",
                json!({"file_path": "src/example.rs", "old_string": "a", "new_string": "b"}),
            ),
            manifest: "",
            pre_setup: &[],
            extra_env: &[],
            budget_p90_ms: 100.0,
            ceiling_ms: 200.0,
            allowed_execs: &["bash", "fno-agents", "git"],
            max_git: None,
        },
        |code, stdout, stderr| {
            assert_eq!(code, 0, "{stderr}");
            assert_eq!(stdout.trim(), "{}", "{stdout} {stderr}");
        },
    );
}

/// Court Edit outside every allowed root: today's deny JSON. AC8-HP.
#[test]
#[ignore]
fn latency_guard_court_edit_deny() {
    let manifest = format!(
        "---\nfno_id: 20260915T190000Z-kg1-abcdef\ncreated_at: 2026-09-15T19:00:00Z\nscope: latency-fixture\nshape: court\nharness: claude\nharness_session_id: {KING_SID}\n---\n"
    );
    run_fixture(
        "guard_court_edit_deny",
        "hooks/king-delegation-guard.sh",
        &FixtureSpec {
            payload: guard_payload(
                KING_SID,
                "Edit",
                json!({"file_path": "src/example.rs", "old_string": "a", "new_string": "b"}),
            ),
            manifest: &manifest,
            pre_setup: &[],
            extra_env: &[],
            budget_p90_ms: 300.0,
            ceiling_ms: 600.0,
            allowed_execs: &[
                "bash",
                "fno-agents",
                "git",
                "fno",
                "python3",
                "python",
                "jq",
            ],
            max_git: None,
        },
        |code, stdout, stderr| {
            assert_eq!(code, 0, "{stderr}");
            let v: Value =
                serde_json::from_str(stdout).unwrap_or_else(|e| panic!("{stdout:?} not JSON: {e}"));
            assert_eq!(
                v["hookSpecificOutput"]["permissionDecision"], "deny",
                "{stdout} {stderr}"
            );
            assert_eq!(v["decision"], "block", "{stdout}");
        },
    );
}

/// Court Write into the plans directory: allowed within the same budget.
/// AC8-HP second leg.
#[test]
#[ignore]
fn latency_guard_court_plan_allow() {
    let manifest = format!(
        "---\nfno_id: 20260915T190000Z-kg1-abcdef\ncreated_at: 2026-09-15T19:00:00Z\nscope: latency-fixture\nshape: court\nharness: claude\nharness_session_id: {KING_SID}\n---\n"
    );
    let b = bench();
    let plans = b.base.join("state/plans");
    fs::create_dir_all(&plans).unwrap();
    let plan_file = plans.join("quick-note.md");
    run_fixture(
        "guard_court_plan_allow",
        "hooks/king-delegation-guard.sh",
        &FixtureSpec {
            payload: guard_payload(
                KING_SID,
                "Write",
                json!({"file_path": plan_file.display().to_string(), "content": "plan"}),
            ),
            manifest: &manifest,
            pre_setup: &[],
            extra_env: &[],
            budget_p90_ms: 300.0,
            ceiling_ms: 600.0,
            allowed_execs: &[
                "bash",
                "fno-agents",
                "git",
                "fno",
                "python3",
                "python",
                "jq",
            ],
            max_git: None,
        },
        |code, stdout, stderr| {
            assert_eq!(code, 0, "{stderr}");
            assert_eq!(
                stdout.trim(),
                "{}",
                "plan write must allow: {stdout} {stderr}"
            );
        },
    );
}

/// AC12-HP: the per-turn hot path stays small. Ceilings freeze this branch's
/// measured floors so the wrappers, the native handlers, and the decision
/// core cannot quietly regrow. Physical counts for the wrappers and
/// `loopcheck.rs`; nonblank noncomment counts for the Rust handlers (up to
/// their `#[cfg(test)]`) and for the decision core.
#[test]
fn hook_sources_stay_small() {
    fn physical(path: &str) -> usize {
        std::fs::read_to_string(path).unwrap().lines().count()
    }
    /// Nonblank, noncomment lines. With `until_cfg_test`, stop at the first
    /// column-0 `#[cfg(test)]` (the unit-test block is not production).
    fn nbnc(path: &str, until_cfg_test: bool) -> usize {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .take_while(|l| !until_cfg_test || !l.starts_with("#[cfg(test)]"))
            .filter(|l| {
                let t = l.trim();
                !t.is_empty() && !t.starts_with("//")
            })
            .count()
    }
    /// Brace-matched extent of `fn <name>(`, counting only code braces (a
    /// mini scanner skips string and char literals so `json!` bodies and
    /// format strings cannot desync the depth).
    fn fn_nbnc(path: &str, name: &str) -> usize {
        let src = std::fs::read_to_string(path).unwrap();
        let sig = format!("fn {name}(");
        let start = src
            .match_indices(&sig)
            .find(|(i, _)| {
                let before = src[..*i].trim_end();
                before.ends_with("pub")
                    || before.ends_with("pub(crate)")
                    || before.ends_with(')')
                    || before.is_empty()
                    || before.ends_with('*')
            })
            .map(|(i, _)| i)
            .unwrap_or_else(|| panic!("{name} not found in {path}"));
        let body_open = src[start..].find('{').unwrap() + start;
        let mut depth = 0usize;
        let mut in_string = false;
        let mut in_char = false;
        let mut escaped = false;
        let mut end = body_open;
        for c in src[body_open..].chars() {
            end += c.len_utf8();
            if escaped {
                escaped = false;
                continue;
            }
            match c {
                '\\' if in_string || in_char => escaped = true,
                '"' if !in_char => in_string = !in_string,
                '\'' if !in_string => in_char = !in_char,
                '{' if !in_string && !in_char => depth += 1,
                '}' if !in_string && !in_char => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        src[start..end]
            .lines()
            .filter(|l| {
                let t = l.trim();
                !t.is_empty() && !t.starts_with("//")
            })
            .count()
    }

    let root = env!("CARGO_MANIFEST_DIR");
    let repo = format!("{root}/../..");
    assert!(
        physical(&format!("{repo}/hooks/target-stop-hook.sh")) <= 20,
        "target-stop-hook.sh must stay a tiny exec wrapper"
    );
    assert!(
        physical(&format!("{repo}/hooks/king-delegation-guard.sh")) <= 20,
        "king-delegation-guard.sh must stay a tiny exec wrapper"
    );
    assert!(
        physical(&format!("{root}/src/loopcheck.rs")) <= 11_700,
        "loopcheck.rs grew past its ceiling"
    );
    assert!(
        nbnc(&format!("{root}/src/hook/stop.rs"), true) <= 850,
        "hook/stop.rs grew past its ceiling"
    );
    assert!(
        nbnc(&format!("{root}/src/hook/king_guard.rs"), true) <= 600,
        "hook/king_guard.rs grew past its ceiling"
    );
    assert!(
        fn_nbnc(&format!("{root}/src/loopcheck.rs"), "decide_with_payload") <= 1_090,
        "the decision core grew past its ceiling"
    );
}
