//! AC5-ERR (x-296f): a malformed override block cannot un-attach a working
//! harness. Own binary: `attach_form` is OnceLock-cached per process (see
//! `attach_override_ac1.rs`).

use std::path::PathBuf;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// AC5-ERR: a MALFORMED block for a harness that has a bundled form degrades
/// to the bundled declaration, never to a lost attach. A typo cannot
/// un-attach a working harness. The well-formed block beside it still lands.
#[test]
fn a_malformed_override_block_cannot_un_attach_a_working_harness() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = std::env::temp_dir().join(format!("fno-x296f-ac5-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        r#"
[harness.claude.attach]
tokens = ["claude", "attach"]

[harness.openclaw.attach]
tokens = ["openclaw", "resume", "{session_id}"]
"#,
    )
    .unwrap();
    let saved = std::env::var_os("FNO_GLOBAL_SETTINGS_PATH");
    std::env::set_var("FNO_GLOBAL_SETTINGS_PATH", dir.join("settings.yaml"));

    let claude = fno::agents_view::attach_form("claude")
        .expect("a typoed override leaves claude's bundled form intact");
    assert_eq!(claude.render("ab12cd34"), ["claude", "attach", "ab12cd34"]);
    assert!(fno::agents_view::attach_form("openclaw").is_some());

    match saved {
        Some(v) => std::env::set_var("FNO_GLOBAL_SETTINGS_PATH", v),
        None => std::env::remove_var("FNO_GLOBAL_SETTINGS_PATH"),
    }
    std::fs::remove_dir_all(&dir).ok();
}
