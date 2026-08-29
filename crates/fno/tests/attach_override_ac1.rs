//! AC1-HP (x-296f): the `[harness.<name>.attach]` config override, through a
//! REAL file read in a fresh process.
//!
//! `attach_form` caches the merged form table in a `OnceLock`, so the override
//! layer can only be exercised end-to-end from a process that has not touched
//! it yet. Each scenario here is its own test binary for exactly that reason;
//! combining them into one file would let the first test's cache answer the
//! rest vacuously.

use std::path::PathBuf;
use std::sync::Mutex;

/// Serializes the env redirect: it is process-global.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn with_global_config(tag: &str, body: &str, f: impl FnOnce()) {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = std::env::temp_dir().join(format!("fno-x296f-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");
    std::fs::write(&config, body).unwrap();
    let saved = std::env::var_os("FNO_GLOBAL_SETTINGS_PATH");
    // The candidate is the sibling of this path, so point it one file over.
    std::env::set_var("FNO_GLOBAL_SETTINGS_PATH", dir.join("settings.yaml"));
    f();
    match saved {
        Some(v) => std::env::set_var("FNO_GLOBAL_SETTINGS_PATH", v),
        None => std::env::remove_var("FNO_GLOBAL_SETTINGS_PATH"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// AC1-HP: a harness fno has never heard of, declared ONLY as a config block,
/// parses into a form, derives an attach id on a thread-shaped row, and
/// reaches Drive. No Rust change, no release.
#[test]
fn a_config_declared_harness_reaches_drive_from_config_alone() {
    with_global_config("ac1", r#"
[harness.openclaw.attach]
tokens = ["openclaw", "resume", "{session_id}"]
pre_exec = ["openclaw", "daemon", "start"]
"#, || {
        let form = fno::agents_view::attach_form("openclaw")
            .expect("a harness declared in config alone gains an attach form");
        let rendered = form.render("sess-9");
        assert_eq!(rendered.first().map(String::as_str), Some("sh"));
        assert!(
            rendered
                .join(" ")
                .contains("; exec 'openclaw' 'resume' 'sess-9'"),
            "action, then exec'd assertion: {rendered:?}"
        );

        let raw = r#"{"agents":[{"name":"ocw","cwd":"/tmp","harness":"openclaw",
            "host_mode":"interactive","short_id":"",
            "harness_session_id":"sess-9","status":"live"}]}"#;
        let rows = fno::agents_view::derive_rows(raw, 0).expect("fixture parses");
        let row = rows.first().unwrap();
        assert_eq!(row.attach_id.as_deref(), Some("sess-9"));
        assert_eq!(
            fno::agents_view::thread_reach(row.harness.as_deref(), row.attach_id.as_deref()),
            fno::proto::Reach::Drive,
            "declared in config alone, so its row reaches Drive"
        );
    });
}
