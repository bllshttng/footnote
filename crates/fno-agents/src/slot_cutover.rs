use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SLOT_CUTOVER_INTERVAL_S: u64 = 120;
const COOLDOWN_S: i64 = 300;

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Skip(&'static str),
    Cutover { from: String, to: String },
}

pub fn decide(
    claude_cap: &Value,
    records: &[Value],
    last_cutover: Option<i64>,
    now: i64,
) -> Decision {
    let shared: Vec<&Value> = records
        .iter()
        .filter(|record| {
            let claude = record.get("harness").and_then(Value::as_str) == Some("claude")
                || record.get("cli").and_then(Value::as_str) == Some("claude");
            let shared_dir = record
                .get("config_dir")
                .and_then(Value::as_str)
                .map_or(true, |dir| dir == "~/.claude");
            claude
                && record.get("auth").and_then(Value::as_str) == Some("managed")
                && record.get("global").and_then(Value::as_bool) == Some(true)
                && shared_dir
        })
        .collect();
    let evidence = claude_cap.get("evidence").and_then(Value::as_object);
    let Some(from) = evidence
        .and_then(|rows| {
            rows.iter()
                .find(|(_, value)| value.as_str() == Some("proven"))
        })
        .map(|(id, _)| id.as_str())
    else {
        return Decision::Skip("slot_unproven");
    };
    if !shared
        .iter()
        .any(|record| record.get("id").and_then(Value::as_str) == Some(from))
    {
        return Decision::Skip("slot_unproven");
    }
    let accounts = claude_cap.get("accounts").and_then(Value::as_object);
    if !matches!(
        accounts
            .and_then(|rows| rows.get(from))
            .and_then(Value::as_str),
        Some("low" | "exhausted")
    ) {
        return Decision::Skip("below_threshold");
    }
    if last_cutover.is_some_and(|last| now.saturating_sub(last) < COOLDOWN_S) {
        return Decision::Skip("cooldown");
    }
    let sources = claude_cap.get("sources").and_then(Value::as_object);
    let target = shared.iter().find_map(|record| {
        let id = record.get("id").and_then(Value::as_str)?;
        (id != from
            && accounts
                .and_then(|rows| rows.get(id))
                .and_then(Value::as_str)
                == Some("ok")
            && sources
                .and_then(|rows| rows.get(id))
                .and_then(Value::as_str)
                == Some("window"))
        .then(|| id.to_string())
    });
    match target {
        Some(to) => Decision::Cutover {
            from: from.to_string(),
            to,
        },
        None => Decision::Skip("no_target"),
    }
}

pub struct Arm {
    last_tick: Mutex<Option<Instant>>,
    in_flight: Arc<AtomicBool>,
    config_cwd: PathBuf,
}

impl Arm {
    pub fn new(config_cwd: PathBuf) -> Self {
        Self {
            last_tick: Mutex::new(None),
            in_flight: Arc::new(AtomicBool::new(false)),
            config_cwd,
        }
    }
}

pub fn maybe_tick(arm: &Arm, home: crate::paths::AgentsHome) {
    let config_cwd = arm.config_cwd.clone();
    {
        let mut last = arm.last_tick.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|tick| tick.elapsed() < Duration::from_secs(SLOT_CUTOVER_INTERVAL_S))
            || arm
                .in_flight
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return;
        }
        *last = Some(Instant::now());
    }
    let flag = Arc::clone(&arm.in_flight);
    std::thread::spawn(move || {
        let _gate = InFlight(flag);
        tick_once(
            &home,
            &config_cwd,
            crate::provider_cap::now_epoch_secs(),
            |title, body| {
                crate::operator_notice::notify_operator(title, body, None);
            },
        );
    });
}

struct InFlight(Arc<AtomicBool>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn tick_once(
    home: &crate::paths::AgentsHome,
    config_cwd: &Path,
    now: i64,
    notify: impl Fn(&str, &str),
) {
    let (acted, skip, detail, notice) = if !crate::agents_config::slot_cutover_enabled(config_cwd) {
        (
            0,
            Some("slot_cutover_off"),
            "slot cutover is disabled".to_string(),
            None,
        )
    } else {
        let refreshed = crate::route_capacity::refresh_usage_readings(
            config_cwd,
            crate::route_capacity::REFRESH_TIMEOUT,
        );
        let capacity =
            crate::route_capacity::capacity(&json!({}), config_cwd, now as f64, refreshed.as_ref());
        let global_ids = crate::agents_config::config_lookup_global(&["accounts", "records"])
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|record| {
                record
                    .get("id")
                    .and_then(|id| id.as_str())
                    .map(str::to_string)
            })
            .collect::<Vec<_>>();
        let records = crate::agents_config::config_lookup(config_cwd, &["accounts", "records"])
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|record| {
                let mut record = serde_json::to_value(record).ok()?;
                let id = record.get("id").and_then(Value::as_str)?.to_string();
                record.as_object_mut()?.insert(
                    "global".to_string(),
                    json!(global_ids.iter().any(|global| global == &id)),
                );
                Some(record)
            })
            .collect::<Vec<_>>();
        match decide(
            &capacity["claude"],
            &records,
            read_last_cutover(home.root()),
            now,
        ) {
            Decision::Skip(reason) => (
                0,
                Some(reason),
                format!("claude slot cutover skipped: {reason}"),
                None,
            ),
            Decision::Cutover { from, to } => {
                let result = crate::provider_cap_verbs::run_fno_output(
                    &["config", "accounts", "use", &to, "--scope", "global"],
                    Some(config_cwd),
                    Duration::from_secs(30),
                );
                if result.is_none() {
                    let detail = format!("could not switch shared Claude slot to {to}");
                    (0, Some("use_failed"), detail.clone(), Some(detail))
                } else {
                    let verified = crate::route_capacity::capacity(
                        &json!({}),
                        config_cwd,
                        now as f64,
                        refreshed.as_ref(),
                    )["claude"]["evidence"][to.as_str()]
                    .as_str()
                        == Some("proven");
                    if !verified {
                        let detail =
                            format!("shared Claude slot did not prove account {to} after switch");
                        (0, Some("cutover_unverified"), detail.clone(), Some(detail))
                    } else {
                        let stamp = json!({"epoch": now, "from": from, "to": to});
                        let path = home.root().join("slot-cutover.json");
                        match crate::king_ledger::write_atomic(
                            &path,
                            &serde_json::to_string(&stamp).unwrap_or_default(),
                        ) {
                            Ok(()) => {
                                let detail = format!("{from} at low -> {to}");
                                (1, None, detail.clone(), Some(detail))
                            }
                            Err(error) => {
                                let detail =
                                    format!("could not record Claude slot cutover: {error}");
                                (0, Some("stamp_failed"), detail.clone(), Some(detail))
                            }
                        }
                    }
                }
            }
        }
    };
    let _ = home.ensure_root();
    let journal = crate::loop_runtime::Journal::new_raw(
        home.events_jsonl(),
        crate::daemon::global_events_path(home),
    );
    crate::tick_ledger::emit_tick(
        &journal,
        "slot_cutover",
        crate::tick_ledger::SCHED_DAEMON,
        acted,
        skip,
        Some(&detail),
        SLOT_CUTOVER_INTERVAL_S,
    );
    if let Some(body) = notice {
        notify("claude slot cutover", &body);
    }
}

fn read_last_cutover(root: &Path) -> Option<i64> {
    let path = root.join("slot-cutover.json");
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&raw)
        .ok()?
        .get("epoch")
        .and_then(Value::as_i64)
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use std::ffi::OsString;

    fn records() -> Vec<Value> {
        vec![
            json!({"id": "readyrule", "harness": "claude", "auth": "managed", "global": true}),
            json!({"id": "makers", "harness": "claude", "auth": "managed", "global": true}),
        ]
    }

    fn capacity(target_source: &str, evidence: &str) -> Value {
        json!({
            "accounts": {"readyrule": "low", "makers": "ok"},
            "sources": {"readyrule": "window", "makers": target_source},
            "evidence": {"readyrule": "proven", "makers": evidence}
        })
    }

    #[test]
    fn a_fresh_shared_managed_target_is_chosen_in_record_order() {
        let mut rows = records();
        rows.insert(
            1,
            json!({"id": "other", "harness": "claude", "auth": "managed", "global": true, "config_dir": "~/.claude-alt"}),
        );
        assert_eq!(
            super::decide(&capacity("window", "mismatch"), &rows, None, 1000),
            super::Decision::Cutover {
                from: "readyrule".into(),
                to: "makers".into()
            }
        );
    }

    #[test]
    fn project_only_accounts_are_not_global_cutover_targets() {
        let mut rows = records();
        rows.insert(
            1,
            json!({"id": "local", "harness": "claude", "auth": "managed", "global": false}),
        );
        let mut cap = capacity("window", "mismatch");
        cap["accounts"]["local"] = json!("ok");
        cap["sources"]["local"] = json!("window");
        assert_eq!(
            super::decide(&cap, &rows, None, 1000),
            super::Decision::Cutover {
                from: "readyrule".into(),
                to: "makers".into()
            }
        );
    }

    #[test]
    fn a_stale_or_unusable_target_is_never_selected() {
        for source in ["stale", "refresh:unauthorized"] {
            assert_eq!(
                super::decide(&capacity(source, "mismatch"), &records(), None, 1000),
                super::Decision::Skip("no_target")
            );
        }
    }

    #[test]
    fn identity_and_cooldown_fail_closed() {
        let mut cap = capacity("window", "mismatch");
        cap["evidence"]["readyrule"] = json!("mismatch");
        assert_eq!(
            super::decide(&cap, &records(), None, 1000),
            super::Decision::Skip("slot_unproven")
        );
        let cap = capacity("window", "mismatch");
        assert_eq!(
            super::decide(&cap, &records(), Some(880), 1000),
            super::Decision::Skip("cooldown")
        );
    }

    #[test]
    fn failed_use_emits_a_failure_tick_without_a_stamp() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let config = dir.path().join("config.toml");
        let global_dir = dir.path().join("global");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(
            global_dir.join("config.toml"),
            "[[accounts.records]]\nid = \"readyrule\"\nharness = \"claude\"\nauth = \"managed\"\n[[accounts.records]]\nid = \"makers\"\nharness = \"claude\"\nauth = \"managed\"\n",
        )
        .unwrap();
        std::fs::write(
            &config,
            format!(
                "state_dir = {:?}\n[slot_cutover]\nenabled = true\n{}",
                state_dir.display().to_string(),
                "[[accounts.records]]\nid = \"readyrule\"\nharness = \"claude\"\nauth = \"managed\"\n[[accounts.records]]\nid = \"makers\"\nharness = \"claude\"\nauth = \"managed\"\n"
            ),
        )
        .unwrap();
        let usage = dir.path().join("usage.json");
        std::fs::write(&usage, "{}").unwrap();
        let providers = state_dir.join("providers");
        std::fs::create_dir_all(&providers).unwrap();
        std::fs::write(providers.join(".active-claude"), "readyrule\n").unwrap();
        let stub = crate::write_exec_stub(
            dir.path(),
            "fno-stub.sh",
            "#!/bin/sh\nif [ \"$3\" = usage ]; then printf '%s\\n' '{\"readyrule\":{\"probed_at\":1000,\"partial\":false,\"windows\":[{\"label\":\"session\",\"used_pct\":95.0,\"resets_at\":null}]},\"makers\":{\"probed_at\":1000,\"partial\":false,\"windows\":[{\"label\":\"session\",\"used_pct\":10.0,\"resets_at\":null}]}}'; exit 0; fi\nexit 1\n",
        );
        let _env = SavedEnv::set(&config, &usage, &stub, &global_dir.join("settings.json"));
        let home = crate::paths::AgentsHome::at(dir.path().join("agents"));
        super::tick_once(&home, dir.path(), 1000, |_, _| {});
        let event = std::fs::read_to_string(home.events_jsonl()).unwrap();
        let event: Value = serde_json::from_str(event.lines().next().unwrap()).unwrap();
        assert_eq!(event["data"]["arm"], "slot_cutover");
        assert_eq!(event["data"]["skip_reason"], "use_failed");
        assert_eq!(event["data"]["acted"], 0);
        assert!(!home.root().join("slot-cutover.json").exists());
    }

    #[test]
    fn disabled_arm_emits_off_tick_without_running_fno() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "schema_version = 1\n").unwrap();
        let global = dir.path().join("global/settings.json");
        std::fs::create_dir_all(global.parent().unwrap()).unwrap();
        std::fs::write(global.with_file_name("config.toml"), "schema_version = 1\n").unwrap();
        let usage = dir.path().join("usage.json");
        std::fs::write(&usage, "{}").unwrap();
        let stub = crate::write_exec_stub(dir.path(), "fno-stub.sh", "#!/bin/sh\nexit 1\n");
        let _env = SavedEnv::set(&config, &usage, &stub, &global);
        let home = crate::paths::AgentsHome::at(dir.path().join("agents"));
        super::tick_once(&home, dir.path(), 1000, |_, _| {});
        let event = std::fs::read_to_string(home.events_jsonl()).unwrap();
        let event: Value = serde_json::from_str(event.lines().next().unwrap()).unwrap();
        assert_eq!(event["data"]["skip_reason"], "slot_cutover_off");
        assert_eq!(event["data"]["acted"], 0);
        assert!(!home.root().join("slot-cutover.json").exists());
    }

    struct SavedEnv {
        config: Option<OsString>,
        state: Option<OsString>,
        bin: Option<OsString>,
        global: Option<OsString>,
    }

    impl SavedEnv {
        fn set(config: &Path, state: &Path, bin: &Path, global: &Path) -> Self {
            let saved = Self {
                config: std::env::var_os("FNO_CONFIG"),
                state: std::env::var_os("FNO_RUNTIME_STATE_PATH"),
                bin: std::env::var_os("FNO_BIN"),
                global: std::env::var_os("FNO_GLOBAL_SETTINGS_PATH"),
            };
            std::env::set_var("FNO_CONFIG", config);
            std::env::set_var("FNO_RUNTIME_STATE_PATH", state);
            std::env::set_var("FNO_BIN", bin);
            std::env::set_var("FNO_GLOBAL_SETTINGS_PATH", global);
            saved
        }
    }

    impl Drop for SavedEnv {
        fn drop(&mut self) {
            for (key, value) in [
                ("FNO_CONFIG", self.config.take()),
                ("FNO_RUNTIME_STATE_PATH", self.state.take()),
                ("FNO_BIN", self.bin.take()),
                ("FNO_GLOBAL_SETTINGS_PATH", self.global.take()),
            ] {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }
}
