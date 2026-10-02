use super::*;
use crate::provenance::TranscriptSource;
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

const KEY: &str = "sk-test-secret-0123456789";

/// A one-connection-per-request HTTP stub: answers each POST with the next
/// canned (status, body) and keeps every request body it read.
fn stub(replies: Vec<(u16, Value)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for (status, body) in replies {
            let Ok((mut conn, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut buf = vec![0; len];
            reader.read_exact(&mut buf).unwrap();
            log.lock().unwrap().push(String::from_utf8(buf).unwrap());
            let text = body.to_string();
            let _ = write!(
                conn,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    (base, seen)
}

fn reply(id: &str, content: Value, stop: &str) -> (u16, Value) {
    (
        200,
        json!({"id": id, "model": "glm-reported", "content": content, "stop_reason": stop,
        "usage": {"input_tokens": 100, "output_tokens": 10}}),
    )
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input})
}

struct Fixture {
    tmp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("cwd")).unwrap();
        std::fs::create_dir_all(root.join("plugin/hooks")).unwrap();
        std::fs::create_dir_all(root.join("plugin/skills/target")).unwrap();
        std::fs::write(
            root.join("plugin/skills/target/SKILL.md"),
            "TARGET SKILL BODY",
        )
        .unwrap();
        // PreToolUse denies any Bash that names graph.db (exit 2); Stop
        // blocks once, then allows (a marker file remembers).
        std::fs::write(
            root.join("plugin/hooks/deny.sh"),
            "#!/bin/sh\nif grep -q graph.db; then echo 'graph.db is protected' >&2; exit 2; fi\n",
        )
        .unwrap();
        std::fs::write(
            root.join("plugin/hooks/stop.sh"),
            format!(
                "#!/bin/sh\ncat >/dev/null\nm={}/stopped\nif [ -f $m ]; then echo '{{}}'; else touch $m; echo '{{\"decision\":\"block\",\"reason\":\"run the tests first\"}}'; fi\n",
                root.display()
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("plugin/hooks/hooks.json"),
            json!({"hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "sh ${CLAUDE_PLUGIN_ROOT}/hooks/deny.sh"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "sh ${CLAUDE_PLUGIN_ROOT}/hooks/stop.sh"}]}],
            }})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join("prices.json"),
            json!({"zai": {"models": {"glm-test": {"cost": {"input": 1.0, "output": 2.0, "cache_read": 0.1}}}}}).to_string(),
        )
        .unwrap();
        Fixture { tmp }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    fn launch(&self, base: &str, manifest: Option<PathBuf>) -> Launch<'static> {
        let cwd: &'static Path = Box::leak(self.p("cwd").into_boxed_path());
        Launch {
            root: self.p("sessions"),
            cwd,
            model: "glm-test",
            endpoint: model::Endpoint {
                base_url: base.to_string(),
                key: KEY.to_string(),
                bearer: true,
                wire: model::Wire::Anthropic,
                provider_id: Some("zai".into()),
                route: "env",
            },
            plugin_root: Some(self.p("plugin")),
            timeout: None,
            node: None,
            parent_session_id: Some("parent-1".into()),
            price_cache: self.p("prices.json"),
            manifest,
        }
    }
}

fn of<'a>(s: &'a Session, ty: &str) -> Vec<&'a Value> {
    s.records.iter().filter(|r| r["type"] == ty).collect()
}

/// One run end to end: skill expansion, a retried empty completion, a hook
/// deny that never starts its tool, a spilled 200 KiB result, a Stop block
/// that continues the loop, the request each turn sent equal to the one
/// rebuilt from the transcript, and the key in no file.
#[test]
fn a_run_records_every_call_before_it_acts() {
    let fx = Fixture::new();
    let big = "head -c 204800 /dev/zero | tr '\\0' x";
    let (base, seen) = stub(vec![
        reply("r0", json!([]), "end_turn"),
        reply(
            "r1",
            json!([
                tool_use("t1", "Bash", json!({"command": "echo x > graph.db"})),
                tool_use("t2", "Bash", json!({"command": big})),
                tool_use(
                    "t3",
                    "Edit",
                    json!({"file_path": "a.txt", "old_string": "a", "new_string": "b"})
                ),
            ]),
            "tool_use",
        ),
        reply(
            "r2",
            json!([{"type": "text", "text": "done once"}]),
            "end_turn",
        ),
        reply("r3", json!([{"type": "text", "text": "done"}]), "end_turn"),
    ]);
    std::fs::write(fx.p("cwd/a.txt"), "a a").unwrap();
    let mut s = start(fx.launch(&base, None)).unwrap();
    s.client.backoff = Duration::ZERO;
    let t = drive(&mut s, "/fno:target x-1234", "operator").unwrap();
    assert_eq!(t.state, "done", "{t:?}");

    // Identity: one v4 id names the dir and every record; seq rises by one.
    let id = s.fno_id();
    assert_eq!(id.len(), 36);
    assert_eq!(s.w.dir().file_name().unwrap().to_string_lossy(), id);
    let on_disk = transcript::read_records(&s.w.transcript_path()).unwrap();
    for (n, r) in on_disk.iter().enumerate() {
        assert_eq!(r["seq"], n as u64);
        assert_eq!(r["session_id"], id.as_str());
        assert_eq!(r["v"], 1);
    }
    let h = &on_disk[0];
    assert_eq!(h["type"], "header");
    assert_eq!(h["data"]["parent_session_id"], "parent-1");
    assert!(h["data"].get("commit").is_some() && h["data"]["fno_version"].is_object());

    // The skill body rode the first request with its argument.
    assert!(
        seen.lock().unwrap()[0].contains("TARGET SKILL BODY")
            && seen.lock().unwrap()[0].contains("ARGUMENTS: x-1234")
    );

    // The empty completion was retried, the attempt recorded and ignorable.
    let attempts = of(&s, "model_attempt");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["ignorable"], true);
    assert_eq!(
        of(&s, "model_response")[0]["data"]["reported_model"],
        "glm-reported"
    );

    // The denied call never started and never ran.
    let uid = |n: usize| of(&s, "tool_call")[n]["data"]["tool_call_uid"].clone();
    let started: Vec<_> = of(&s, "tool_start")
        .iter()
        .map(|r| r["data"]["tool_call_uid"].clone())
        .collect();
    assert!(!started.contains(&uid(0)));
    assert!(!fx.p("cwd/graph.db").exists());
    let dec = of(&s, "effect_decision");
    assert_eq!(dec[0]["data"]["verdict"], "deny");
    assert!(dec[0]["data"]["rule"].as_str().unwrap().contains("deny.sh"));
    let results = of(&s, "tool_result");
    assert_eq!(results[0]["data"]["disposition"], "none");

    // 200 KiB spilled whole; the record keeps size, sha256 and path.
    let r1 = &results[1]["data"];
    assert_eq!(r1["disposition"], "applied");
    assert_eq!(r1["size"], 204800);
    let spill = std::fs::read(r1["spill_path"].as_str().unwrap()).unwrap();
    assert_eq!(spill.len(), 204800);
    assert_eq!(r1["sha256"], transcript::sha256_hex(&spill));

    // A non-unique Edit refused and left the file alone.
    assert_eq!(results[2]["data"]["is_error"], true);
    assert_eq!(std::fs::read_to_string(fx.p("cwd/a.txt")).unwrap(), "a a");

    // Stop blocked once: its reason became the next user input.
    assert!(of(&s, "user_input")
        .iter()
        .any(|r| r["data"]["text"] == "run the tests first"));
    assert_eq!(of(&s, "terminal")[0]["data"]["state"], "done");

    // Model-visible equals logged: every request rebuilds from the records before it.
    for (n, req) in on_disk
        .iter()
        .filter(|r| r["type"] == "model_request")
        .enumerate()
    {
        let at = req["seq"].as_u64().unwrap() as usize;
        let rebuilt = resume::build_request(&on_disk[..at]);
        assert_eq!(
            req["data"]["body_sha256"],
            transcript::sha256_hex(rebuilt.as_bytes())
        );
        assert_eq!(
            seen.lock().unwrap()[if n == 0 { 0 } else { n + 1 }],
            rebuilt
        );
    }

    // The key is in no file; diag lines carry run and session ids.
    s.w.diag("info", &format!("probe {KEY}"));
    for f in ["transcript.jsonl", "diag.log"] {
        let text = std::fs::read_to_string(s.w.dir().join(f)).unwrap();
        assert!(!text.contains(KEY), "{f} leaks the key");
    }
    let diag = std::fs::read_to_string(s.w.dir().join("diag.log")).unwrap();
    assert!(diag.contains(&format!("session={id}")) && diag.contains("run="));

    // The index holds one content-free row per record.
    let conn = rusqlite::Connection::open(s.w.dir().join("index.db")).unwrap();
    let rows: i64 = conn
        .query_row(
            "SELECT count(*) FROM events WHERE session_id = ?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows as usize, on_disk.len());
}

/// An existing id refuses a create; a live writer refuses a second one.
fn one_writer_per_session() {
    let fx = Fixture::new();
    let dir = fx.p("sessions/_none/abc");
    let w = transcript::Writer::create(&dir, "abc").unwrap();
    assert!(transcript::Writer::create(&dir, "abc")
        .err()
        .unwrap()
        .contains("already exists"));
    let err = transcript::Writer::open(&dir, "abc").err().unwrap();
    assert!(err.contains("live writer"), "{err}");
    drop(w);
    // A sibling test's fork can hold the inherited lock fd until its exec.
    let reopened = (0..100).any(|_| {
        let ok = transcript::Writer::open(&dir, "abc").is_ok();
        if !ok {
            std::thread::sleep(Duration::from_millis(20));
        }
        ok
    });
    assert!(reopened, "the lock outlived its writer");
}

/// Resume never re-runs a started effect-capable call; it re-runs a call
/// that never started and a started read-only one.
#[test]
fn resume_settles_dead_calls_by_effect() {
    one_writer_per_session();
    let fx = Fixture::new();
    std::fs::write(fx.p("cwd/r.txt"), "hello").unwrap();
    let (base, _) = stub(vec![]);
    let mut s = start(fx.launch(&base, None)).unwrap();
    let call = |s: &mut Session, uid: &str, name: &str, input: Value, started: bool| {
        s.append(
            "tool_call",
            json!({"tool_call_uid": uid, "provider_tool_call_id": uid, "name": name,
            "input": input, "raw_input": "", "parse_error": null}),
        )
        .unwrap();
        if started {
            s.append("tool_start", json!({"tool_call_uid": uid}))
                .unwrap();
        }
    };
    call(
        &mut s,
        "u-bash",
        "Bash",
        json!({"command": "touch ran"}),
        true,
    );
    call(
        &mut s,
        "u-read",
        "Read",
        json!({"file_path": "r.txt"}),
        true,
    );
    call(
        &mut s,
        "u-write",
        "Write",
        json!({"file_path": "w.txt", "content": "w"}),
        false,
    );
    let (dir, id) = (s.w.dir().to_path_buf(), s.fno_id());
    drop(s);
    let s = reopen(&dir, &id, fx.launch(&base, None)).unwrap();
    let by = |uid: &str| {
        of(&s, "tool_result")
            .into_iter()
            .find(|r| r["data"]["tool_call_uid"] == uid)
            .unwrap()["data"]
            .clone()
    };
    assert_eq!(by("u-bash")["disposition"], "unknown");
    assert!(!fx.p("cwd/ran").exists());
    assert!(by("u-read")["model_text"]
        .as_str()
        .unwrap()
        .contains("hello"));
    assert_eq!(by("u-write")["disposition"], "applied");
    assert!(fx.p("cwd/w.txt").exists());
}

/// A dollar cap the next call would cross sends nothing; a cap with no
/// price refuses to start; past 80% of the window the run compacts with
/// the plan's acceptance criteria inline.
#[test]
fn budget_stops_before_the_call_and_compaction_keeps_the_plan() {
    let fx = Fixture::new();
    let manifest = fx.p("target-state.md");
    std::fs::write(
        fx.p("plan.md"),
        "# P\n\n## Acceptance Criteria\n\nAC1: it works\n\n## Other\n\nx\n",
    )
    .unwrap();
    std::fs::write(
        &manifest,
        format!(
            "budget_cost_cap_usd: 0.0000001\nplan_path: {}\n",
            fx.p("plan.md").display()
        ),
    )
    .unwrap();
    let (base, seen) = stub(vec![]);
    let mut s = start(fx.launch(&base, Some(manifest.clone()))).unwrap();
    assert_eq!(drive(&mut s, "go", "operator").unwrap().state, "budget");
    assert!(seen.lock().unwrap().is_empty());
    assert!(of(&s, "model_request").is_empty());

    let mut no_price = fx.launch(&base, Some(manifest.clone()));
    no_price.model = "unpriced";
    assert!(start(no_price).err().unwrap().contains("no price"));

    std::fs::write(
        &manifest,
        format!("plan_path: {}\n", fx.p("plan.md").display()),
    )
    .unwrap();
    let (base, seen) = stub(vec![reply(
        "r1",
        json!([{"type": "text", "text": "ok"}]),
        "end_turn",
    )]);
    let mut s = start(fx.launch(&base, Some(manifest))).unwrap();
    s.submit("first ask", "operator").unwrap();
    s.append(
        "usage",
        json!({"input_tokens": 190_000, "output_tokens": 0}),
    )
    .unwrap();
    std::fs::write(fx.p("stopped"), "").unwrap();
    s.run_turns().unwrap();
    let c = &of(&s, "compaction")[0]["data"];
    assert_eq!(c["trigger"], "auto_80pct");
    assert!(c["plan_anchor_sha256"].is_string());
    assert!(seen.lock().unwrap()[0].contains("AC1: it works"));
}

/// The intel source lists a footnote session with its turns and calls; an
/// unknown non-ignorable record type refuses the read.
#[test]
fn readers_see_the_transcript_and_refuse_unknown_types() {
    endpoint_refuses_a_subscription_login();
    let fx = Fixture::new();
    let dir = fx.p("sessions/_none/s1");
    let mut w = transcript::Writer::create(&dir, "s1").unwrap();
    w.append("header", json!({"cwd": fx.p("cwd")}), false)
        .unwrap();
    w.append(
        "user_input",
        json!({"text": "hi", "origin": "operator"}),
        false,
    )
    .unwrap();
    w.append("tool_call", json!({"name": "Read"}), false)
        .unwrap();
    w.append("future_thing", json!({}), true).unwrap();
    let src = source::FootnoteSource {
        sessions_root: fx.p("sessions"),
        roots: None,
    };
    let files = src.sessions(0);
    assert_eq!(files.len(), 1);
    let raw = src.read(&files[0]);
    assert_eq!(src.turns(&raw).len(), 1);
    assert_eq!(src.tool_uses(&raw), 1);
    assert!(transcript::read_records(&w.transcript_path()).is_ok());
    w.append("model_response", json!({"reported_model": "glm-x"}), false)
        .unwrap();
    w.append(
        "usage",
        json!({"input_tokens": 7, "output_tokens": 3}),
        false,
    )
    .unwrap();
    let payload = json!({"lane": {"name": "f", "harness": "footnote", "model": "glm-x"},
        "workdir": fx.p("cwd"), "started_epoch": 0.0, "footnote_sessions_root": fx.p("sessions")});
    let seen = crate::eval_attempt::observe(&payload);
    assert_eq!(
        (
            seen["lane_status"].as_str(),
            seen["usage"]["input"].as_u64()
        ),
        (Some("ok"), Some(7))
    );
    w.append("future_thing", json!({}), false).unwrap();
    assert!(transcript::read_records(&w.transcript_path())
        .unwrap_err()
        .contains("future_thing"));
}

/// A Claude.ai login never reaches api.anthropic.com; a route env does.
fn endpoint_refuses_a_subscription_login() {
    let cwd = Path::new("/");
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| pairs.iter().find(|p| p.0 == k).map(|p| p.1.to_string())
    };
    let err = model::resolve_endpoint(
        cwd,
        &env(&[
            ("ANTHROPIC_BASE_URL", "https://api.anthropic.com"),
            ("ANTHROPIC_AUTH_TOKEN", "oauth"),
        ]),
    )
    .unwrap_err();
    assert!(err.contains("Claude Code lane"), "{err}");
    let ep = model::resolve_endpoint(
        cwd,
        &env(&[
            ("ANTHROPIC_BASE_URL", "https://api.z.ai/api/anthropic"),
            ("ANTHROPIC_AUTH_TOKEN", "k"),
            ("FNO_ROUTE_PROVIDER", "zai"),
        ]),
    )
    .unwrap();
    assert_eq!(
        (ep.wire, ep.provider_id.as_deref(), ep.bearer),
        (model::Wire::Anthropic, Some("zai"), true)
    );
}
