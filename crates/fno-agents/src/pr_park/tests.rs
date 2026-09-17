use super::*;

fn write(path: &Path, text: &str) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

/// An RFC3339 UTC stamp `hours` before now. Age-gated fixtures must ride the
/// clock: a literal date crosses the 24h park bound a day after it is written
/// and flips the sweep under the test.
fn iso_hours_ago(hours: u64) -> String {
    let t = now_secs().saturating_sub(hours * 3600);
    let (year, month, day, hour, min, sec) = crate::events::civil_from_unix(t);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// The fixture store: one open-PR park, one finished delivery park, one
/// foreign park. Three buckets, one of each.
fn fixture(paths: &Paths) {
    let fresh = iso_hours_ago(2);
    let state_text = r#"{
  "owner/repo#101": {
    "last_seen_state": "OPEN",
    "retries": 3,
    "parked": "retries-exhausted",
    "last_polled_at": "{fresh}"
  },
  "other/repo#55": {
    "last_seen_state": "OPEN",
    "retries": 3,
    "parked": "max-age",
    "last_polled_at": "{fresh}"
  }
}"#
    .replace("{fresh}", &fresh);
    write(&paths.state, &state_text);
    let delivery_text = r#"{
  "owner/repo#42": {
    "last_seen_state": "MERGED",
    "retries": 3,
    "parked": "retries-exhausted",
    "last_polled_at": "{fresh}"
  }
}"#
    .replace("{fresh}", &fresh);
    write(&paths.delivery, &delivery_text);
    // The open row's failure detail, as the merge phase recorded it.
    write(
        &paths.events,
        r#"{"ts":"2026-09-16T15:29:00Z","type":"unrelated","data":{"pr":101}}
{"ts":"2026-09-16T15:29:30Z","type":"merge_grant_execution","data":{"phase":"failed","actor":"pr-watch","pr":101,"exit_code":1}}
"#,
    );
    write(
        &paths.err_log,
        r#"{"pr":101,"outcome":"failed","reason":"failed: checks are red; require_checks_pass forbids merging without green","strategy":"merge"}
"#,
    );
}

/// A probe answering the same head on every call.
fn ctx(paths: Paths) -> Ctx {
    ctx_ans(paths, "head1", "OPEN")
}

/// A probe whose answer is fixed per construction; the sweep test moves the
/// head by building a fresh Ctx with the next answer.
fn ctx_ans(paths: Paths, head: &'static str, state: &'static str) -> Ctx {
    Ctx {
        paths,
        slug: "owner/repo".to_string(),
        entries: vec![json!({
            "id": "x-open",
            "status": "in_review",
            "pr_number": 101,
            "pr_url": "https://github.com/owner/repo/pull/101"
        })],
        head: Box::new(move |pr| {
            if pr == 101 {
                Ok((head.to_string(), state.to_string()))
            } else {
                Err("probe failed".to_string())
            }
        }),
    }
}

fn tmp_paths(tag: &str) -> Paths {
    let dir = std::env::temp_dir().join(format!("pr-park-test-{tag}-{}", std::process::id()));
    Paths {
        state: dir.join("pr-watcher-state.json"),
        delivery: dir.join("pr-watcher-state-delivery.json"),
        events: dir.join("events.jsonl"),
        err_log: dir.join("pr-watcher.err.log"),
    }
}

#[test]
fn list_buckets_open_finished_and_foreign() {
    let paths = tmp_paths("list");
    fixture(&paths);
    let rows = list_rows(&ctx(paths.clone()));
    assert_eq!(rows.len(), 3);
    let by_key = |k: &str| rows.iter().find(|r| r.key == k).unwrap().clone();
    assert_eq!(by_key("owner/repo#101").bucket, "open");
    assert_eq!(by_key("owner/repo#42").bucket, "finished");
    assert_eq!(by_key("other/repo#55").bucket, "foreign");
    // The open row resolves its node and its stored failure detail.
    let open = by_key("owner/repo#101");
    assert_eq!(open.node, "x-open");
    assert_eq!(open.node_status, "in_review");
    assert!(
        open.reason_detail.contains("failed"),
        "{}",
        open.reason_detail
    );
    // Clean up after the reason-detail tail read.
    let _ = std::fs::remove_dir_all(paths.state.parent().unwrap());
}

#[test]
fn unpark_clears_parked_resets_retries_and_emits() {
    let paths = tmp_paths("unpark");
    fixture(&paths);
    let n = unpark(&ctx(paths.clone()), Some("owner/repo#101"), false).unwrap();
    assert_eq!(n, 1);
    let data: Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.state).unwrap()).unwrap();
    let entry = &data["owner/repo#101"];
    assert!(entry["parked"].is_null());
    assert_eq!(entry["retries"], json!(0));
    // The other rows keep their parks, and one unpark row was emitted.
    assert!(!data["other/repo#55"]["parked"].is_null());
    let events = std::fs::read_to_string(&paths.events).unwrap();
    assert!(events.contains("\"pr_watch_unparked\""), "{events}");
    assert!(events.contains("\"by\":\"manual\""), "{events}");
    let _ = std::fs::remove_dir_all(paths.state.parent().unwrap());
}

#[test]
fn unpark_all_open_leaves_delivery_and_foreign() {
    let paths = tmp_paths("all-open");
    fixture(&paths);
    let n = unpark(&ctx(paths.clone()), None, true).unwrap();
    assert_eq!(n, 1);
    let data: Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.state).unwrap()).unwrap();
    assert!(data["owner/repo#101"]["parked"].is_null());
    assert!(!data["other/repo#55"]["parked"].is_null());
    let delivery: Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.delivery).unwrap()).unwrap();
    assert!(!delivery["owner/repo#42"]["parked"].is_null());
    let _ = std::fs::remove_dir_all(paths.state.parent().unwrap());
}

#[test]
fn sweep_unparks_on_head_change_and_holds_on_same_head() {
    let paths = tmp_paths("sweep-head");
    fixture(&paths);
    // Baseline round: records the head it sees and parks nothing.
    let r = sweep(&ctx(paths.clone())).unwrap();
    assert_eq!(r.unparked, 0);
    let data: Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.state).unwrap()).unwrap();
    assert_eq!(data["owner/repo#101"]["parked_head"], json!("head1"));
    assert!(!data["owner/repo#101"]["parked"].is_null());
    // Same head: stays parked.
    let _ = sweep(&ctx(paths.clone())).unwrap();
    let data: Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.state).unwrap()).unwrap();
    assert!(!data["owner/repo#101"]["parked"].is_null());
    // A pushed head: un-parked.
    let _ = sweep(&ctx_ans(paths.clone(), "head2", "OPEN")).unwrap();
    let data: Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.state).unwrap()).unwrap();
    assert!(data["owner/repo#101"]["parked"].is_null());
    let events = std::fs::read_to_string(&paths.events).unwrap();
    assert!(events.contains("\"by\":\"sweep\""), "{events}");
    let _ = std::fs::remove_dir_all(paths.state.parent().unwrap());
}

#[test]
fn sweep_unparks_a_day_old_park_without_probing_and_handles_finished() {
    let paths = tmp_paths("sweep-age");
    write(
        &paths.state,
        r#"{
  "owner/repo#101": {
    "last_seen_state": "OPEN",
    "retries": 3,
    "parked": "retries-exhausted",
    "last_polled_at": "2026-09-14T15:30:00+00:00"
  }
}"#,
    );
    fixture_delivery_finished(&paths);
    let r = sweep(&ctx(paths.clone())).unwrap();
    assert_eq!(r.unparked, 1);
    assert_eq!(r.handled, 1);
    let data: Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.state).unwrap()).unwrap();
    assert!(data["owner/repo#101"]["parked"].is_null());
    let delivery: Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.delivery).unwrap()).unwrap();
    assert_eq!(delivery["owner/repo#42"]["parked"], json!(HANDLED));
    let _ = std::fs::remove_dir_all(paths.state.parent().unwrap());
}

fn fixture_delivery_finished(paths: &Paths) {
    write(
        &paths.delivery,
        r#"{
  "owner/repo#42": {
    "last_seen_state": "MERGED",
    "retries": 3,
    "parked": "retries-exhausted",
    "last_polled_at": "2026-09-16T15:30:00+00:00"
  }
}"#,
    );
}

#[test]
fn a_row_that_merged_while_parked_is_handled_not_resumed() {
    let paths = tmp_paths("sweep-merged");
    fixture(&paths);
    // Baseline first, then a probe that answers MERGED.
    let _ = sweep(&ctx(paths.clone())).unwrap();
    let mut merged = ctx(paths.clone());
    merged.head = Box::new(|_pr| Ok(("head9".to_string(), "MERGED".to_string())));
    let r = sweep(&merged).unwrap();
    assert_eq!(r.handled, 1);
    let data: Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.state).unwrap()).unwrap();
    assert_eq!(data["owner/repo#101"]["parked"], json!(HANDLED));
    let _ = std::fs::remove_dir_all(paths.state.parent().unwrap());
}

#[test]
fn resolve_follows_a_configured_state_dir() {
    // The store must follow the same `state_dir` the Python watcher reads,
    // or the king's parked board and the daemon sweep watch an empty file
    // while the parks live under the override.
    let dir = std::env::temp_dir().join(format!(
        "pr-park-test-resolve-{}-{}",
        std::process::id(),
        now_secs()
    ));
    let repo = dir.join("repo");
    write(
        &repo.join(".fno/config.toml"),
        "state_dir = \"alt-state\"\n",
    );
    // A relative config value resolves against the repo, the way a watcher
    // started in that checkout resolves it.
    let p = Paths::resolve(&repo);
    assert_eq!(
        p.state,
        repo.join("alt-state").join("pr-watcher-state.json")
    );
    assert_eq!(p.events, repo.join("alt-state").join("events.jsonl"));
    write(
        &repo.join(".fno/config.toml"),
        "state_dir = \"/tmp/park-alt-abs\"\n",
    );
    let p = Paths::resolve(&repo);
    assert_eq!(
        p.state,
        Path::new("/tmp/park-alt-abs").join("pr-watcher-state.json")
    );
    // No key, no divergence: the home default, so a default install (and a
    // test env) resolves exactly where the watcher already writes.
    write(&repo.join(".fno/config.toml"), "unrelated = true\n");
    assert_eq!(Paths::resolve(&repo).state, Paths::from_home().state);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mutations_hold_the_lock_file_the_watcher_takes() {
    // The `.lock` files are the cross-language serialization point: the
    // Python WatermarkStore.set() takes the same flock, so a sweep rewrite
    // can never interleave with a tick write.
    let paths = tmp_paths("lock");
    fixture(&paths);
    let _ = unpark(&ctx(paths.clone()), Some("owner/repo#101"), false).unwrap();
    assert!(crate::gh_budget::lock_path(&paths.state).exists());
    assert!(crate::gh_budget::lock_path(&paths.delivery).exists());
    let _ = std::fs::remove_dir_all(paths.state.parent().unwrap());
}
