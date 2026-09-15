use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Bench {
    repo: PathBuf,
    root: PathBuf,
    out: PathBuf,
    env: BTreeMap<String, String>,
    events: PathBuf,
    global: PathBuf,
    state: PathBuf,
    samples: Vec<Value>,
}

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn executable(path: &Path, body: &str) {
    write(path, body);
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn cpu() -> (u64, u64) {
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
}

fn idle() -> Vec<Value> {
    let mut readings = Vec::new();
    let mut quiet = 0;
    for _ in 0..120 {
        let before = cpu();
        std::thread::sleep(Duration::from_secs(2));
        let after = cpu();
        let busy = 1.0 - (after.1 - before.1) as f64 / (after.0 - before.0).max(1) as f64;
        readings.push(json!({"cpu_busy":busy,"load":fs::read_to_string("/proc/loadavg").unwrap()}));
        quiet = if busy < 0.10 { quiet + 1 } else { 0 };
        if quiet == 3 {
            return readings;
        }
    }
    panic!("runner did not reach three consecutive CPU-busy readings below 10%");
}

impl Bench {
    fn run(&mut self, name: &str, args: Vec<String>, payload: Option<Value>, n: usize) {
        for i in 0..(n + 10) {
            write(&self.events, "");
            write(&self.global, "");
            let start = Instant::now();
            let mut command = Command::new(&args[0]);
            command
                .args(&args[1..])
                .current_dir(&self.repo)
                .envs(&self.env);
            for (k, _) in std::env::vars() {
                if (k.starts_with("FNO_")
                    || k.starts_with("CODEX_")
                    || k.starts_with("CLAUDE_")
                    || k.starts_with("GEMINI_")
                    || k.starts_with("OPENCODE_"))
                    && !self.env.contains_key(&k)
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
            let input = payload.as_ref().map(Value::to_string).unwrap_or_default();
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
                stdout.read_to_string(&mut s).unwrap();
                s
            });
            let err_reader = std::thread::spawn(move || {
                let mut s = String::new();
                stderr.read_to_string(&mut s).unwrap();
                s
            });
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if start.elapsed() > Duration::from_secs(60) {
                    child.kill().unwrap();
                    panic!("{name} exceeded 60 seconds");
                }
                std::thread::sleep(Duration::from_micros(250));
            };
            let stdout = out_reader.join().unwrap();
            let stderr = err_reader.join().unwrap();
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            let events = fs::read_to_string(&self.events).unwrap_or_default();
            if name.starts_with("guard_") {
                let value: Value = serde_json::from_str(&stdout).expect("guard JSON");
                assert!(status.success(), "{name}: {stderr}");
                if name == "guard_court_deny" {
                    assert_eq!(
                        value["hookSpecificOutput"]["permissionDecision"], "deny",
                        "{stderr}"
                    );
                } else {
                    assert_eq!(value, json!({}), "{name}: {stderr}");
                }
                assert!(
                    events.contains("guard_decision"),
                    "guard telemetry absent: {name}: {stderr}"
                );
            } else if name.starts_with("target_") || name.starts_with("core_") {
                let value: Value = serde_json::from_str(&stdout).expect("Stop JSON");
                assert_eq!(value["decision"], "block", "{name}: {stdout} {stderr}");
                assert!(
                    events.contains("loop_check"),
                    "loop telemetry absent: {name}"
                );
            } else if name == "visitor" {
                assert!(
                    status.success()
                        && stdout.trim().is_empty()
                        && stderr.contains("visitor allowed"),
                    "{stdout} {stderr}"
                );
            } else {
                assert!(status.success(), "{name}: {stderr}");
            }
            let row = json!({"case":name,"iteration":i,"warmup":i<10,"ms":elapsed,"exit":status.code(),"stdout":stdout,"stderr":stderr,"event_bytes":events.len(),"load":fs::read_to_string("/proc/loadavg").unwrap()});
            writeln!(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.out.join("samples.jsonl"))
                    .unwrap(),
                "{row}"
            )
            .unwrap();
            if i >= 10 {
                self.samples.push(row);
            }
        }
        println!("{name}: {n} measured samples, output and telemetry verified");
    }

    fn hook(&mut self, name: &str, script: &str, payload: Value, n: usize) {
        self.run(
            name,
            vec!["bash".into(), self.root.join(script).display().to_string()],
            Some(payload),
            n,
        );
    }
}

fn main() {
    let root = fs::canonicalize(std::env::args().nth(1).expect("repository path")).unwrap();
    let out = root.join("hook-latency-results");
    fs::create_dir_all(&out).unwrap();
    let base = std::env::temp_dir().join(format!("fno-idle-latency-{}", std::process::id()));
    let repo = base.join("repo");
    fs::create_dir_all(&repo).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .arg(&repo)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "Fixture"
        ])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    for i in 0..15 {
        assert!(Command::new("git")
            .args(["worktree", "add", "--detach", "-q"])
            .arg(base.join(format!("worktree-{i}")))
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
    }
    let spaces = base.join("spaces");
    let space = spaces.join(repo.display().to_string().replace('/', "-"));
    let agents_home = base.join("state/agents");
    let events = space.join("events.jsonl");
    let global = base.join("state/global-events.jsonl");
    let config = base.join("config.toml");
    let state = space.join("target-state.md");
    let king = "00000000-0000-4000-8000-000000000001";
    let target = "00000000-0000-4000-8000-000000000002";
    let visitor = "00000000-0000-4000-8000-000000000003";
    let mut rows = Vec::new();
    for i in 0..200 {
        rows.push(json!({"name":format!("fixture-{i}"),"cwd":repo,"log_path":"","harness":"claude","harness_session_id":format!("session-{i}"),"status":"live"}));
    }
    rows.push(json!({"name":"fixture-court","cwd":repo,"log_path":"","harness":"claude","harness_session_id":king,"status":"live","crown_level":2,"crown_scope":"idle-fixture"}));
    write(
        &agents_home.join("registry.json"),
        &json!({"schema_version":26,"agents":rows}).to_string(),
    );
    write(&space.join("kings/idle-fixture.md"), &format!("---\nfno_id: 20260915T190000Z-kg1-abcdef\ncreated_at: 2026-09-15T19:00:00Z\nscope: idle-fixture\nshape: court\nharness: claude\nharness_session_id: {king}\n---\n"));
    write(&config, &format!("state_dir = {:?}\n[paths]\nspaces_dir = {:?}\nplans_dir = {:?}\n[king]\nimplementation_guard = \"refuse\"\n[review]\nrequired_bots = []\nreviewers = []\n",base.join("state"),spaces,base.join("plans")));
    let gh = base.join("gh-fixture");
    executable(&gh, "#!/bin/sh\ncase \"$1 $2\" in\n \"--version \") echo 'gh version fixture';;\n \"api rate_limit\") echo '{\"resources\":{\"graphql\":{\"remaining\":5000,\"reset\":1999999999,\"limit\":5000}}}';;\n \"pr view\") echo 'no pull requests found for branch' >&2; exit 1;;\n *) echo '{}';;\nesac\n");
    let real = root.join("crates/fno-agents/target/release/fno-agents");
    let shim = base.join("agents-with-event-pins");
    executable(&shim, &format!("#!/bin/sh\nif [ \"$1\" = loop-check ]; then exec {:?} \"$@\" --events {:?} --global-events {:?} --settings {:?} --global-settings {:?} --ledger {:?} --gh-budget-ledger {:?}; fi\nexec {:?} \"$@\"\n",real,events,global,config,config,base.join("state/ledger.json"),base.join("state/gh-budget.json"),real));
    let mut env = BTreeMap::new();
    for (k, v) in [
        ("FNO_CONFIG", config.clone()),
        ("FNO_AGENTS_HOME", agents_home),
        ("FNO_SPACES_DIR", spaces),
        ("FNO_HOME", base.join("state")),
        ("FNO_CLAIMS_ROOT", base.join("claims")),
        ("FNO_BUS_DIR", base.join("state/bus")),
        ("FNO_INBOX_ROOT", base.join("state/inbox")),
        ("EVENTS_FILE", events.clone()),
        ("GLOBAL_EVENTS_PATH", global.clone()),
        ("FNO_AGENTS_BIN", shim),
        ("FNO_LOOPCHECK_GH_BIN", gh),
        ("CLAUDE_PLUGIN_ROOT", root.clone()),
    ] {
        env.insert(k.into(), v.display().to_string());
    }
    env.insert("CLAUDECODE".into(), "1".into());
    env.insert("FNO_HARNESS".into(), "claude".into());
    for sid in [visitor, target] {
        write(&base.join(format!("{sid}.jsonl")), "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Work continues.\"}]}}\n");
    }
    let mut b = Bench {
        repo: repo.clone(),
        root: root.clone(),
        out: out.clone(),
        env,
        events,
        global,
        state,
        samples: Vec::new(),
    };
    b.run(
        "bootstrap",
        vec![
            "fno".into(),
            "config".into(),
            "get".into(),
            "king.implementation_guard".into(),
        ],
        None,
        1,
    );
    let quiet = idle();
    write(&out.join("environment.json"),&json!({"idle_readings":quiet,"cpus":std::thread::available_parallelism().unwrap().get(),"runner_os":std::env::consts::OS,"revision":std::env::var("GITHUB_SHA").ok(),"registry_rows":201,"worktrees":16,"fixture_root":base}).to_string());
    b.samples.clear();
    let guard = |sid: &str, tool: &str, input: Value| json!({"session_id":sid,"cwd":repo,"transcript_path":"","hook_event_name":"PreToolUse","tool_name":tool,"tool_input":input});
    b.run(
        "shell_exit",
        vec!["bash".into(), "-c".into(), "exit 0".into()],
        None,
        100,
    );
    b.run(
        "native_registry",
        vec![real.display().to_string(), "registry-json".into()],
        None,
        100,
    );
    b.hook(
        "guard_uncrowned",
        "hooks/king-delegation-guard.sh",
        guard(visitor, "Bash", json!({"command":"git status --short"})),
        50,
    );
    b.hook(
        "guard_court_read",
        "hooks/king-delegation-guard.sh",
        guard(king, "Bash", json!({"command":"git status --short"})),
        50,
    );
    b.hook(
        "guard_court_deny",
        "hooks/king-delegation-guard.sh",
        guard(
            king,
            "Edit",
            json!({"file_path":"src/example.rs","old_string":"a","new_string":"b"}),
        ),
        50,
    );
    let stop = |sid: &str, message: &str| json!({"session_id":sid,"cwd":repo,"transcript_path":base.join(format!("{sid}.jsonl")),"last_assistant_message":message,"hook_event_name":"Stop","stop_hook_active":false});
    b.hook(
        "visitor",
        "hooks/target-stop-hook.sh",
        stop(visitor, "Work continues."),
        50,
    );
    write(&b.state,&format!("---\nfno_id: 20260915T190000Z-kt1-abcdef\nharness_session_id: {target}\nharness: claude\ncreated_at: 2026-09-15T19:00:00Z\nattended: true\nno_external: true\n---\n"));
    b.hook(
        "target_no_intent",
        "hooks/target-stop-hook.sh",
        stop(target, "Work continues."),
        50,
    );
    b.hook(
        "target_promise",
        "hooks/target-stop-hook.sh",
        stop(target, "<promise>COMPLETE</promise>"),
        50,
    );
    for (name, message) in [
        ("core_no_intent", "Work continues."),
        ("core_promise", "<promise>COMPLETE</promise>"),
    ] {
        b.run(
            name,
            vec![
                real.display().to_string(),
                "loop-check".into(),
                "--state".into(),
                b.state.display().to_string(),
                "--cwd".into(),
                repo.display().to_string(),
                "--transcript".into(),
                base.join(format!("{target}.jsonl")).display().to_string(),
                "--events".into(),
                b.events.display().to_string(),
                "--global-events".into(),
                b.global.display().to_string(),
                "--settings".into(),
                config.display().to_string(),
                "--global-settings".into(),
                config.display().to_string(),
                "--hook-input-stdin".into(),
            ],
            Some(stop(target, message)),
            50,
        );
    }
    let mut grouped: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for sample in &b.samples {
        grouped
            .entry(sample["case"].as_str().unwrap().into())
            .or_default()
            .push(sample["ms"].as_f64().unwrap());
    }
    let mut summary = BTreeMap::new();
    for (name, mut values) in grouped {
        values.sort_by(f64::total_cmp);
        let n = values.len();
        let p50 = if n % 2 == 0 {
            (values[n / 2 - 1] + values[n / 2]) / 2.0
        } else {
            values[n / 2]
        };
        summary.insert(
            name,
            json!({"n":n,"p50_ms":p50,"p90_ms":values[(n*9).div_ceil(10)-1],"max_ms":values[n-1]}),
        );
    }
    let summary = serde_json::to_string_pretty(&summary).unwrap();
    write(&out.join("summary.json"), &summary);
    println!("{summary}");
}
