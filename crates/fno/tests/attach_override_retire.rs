//! The explicit-retirement override (x-296f). Own binary: `attach_form` is
//! OnceLock-cached per process (see `attach_override_ac1.rs`).

use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// An explicit kind = "unsupported" override RETIRES a bundled form: the
/// parsable way to say "this harness cannot attach".
#[test]
fn an_explicit_unsupported_override_retires_a_bundled_form() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = std::env::temp_dir().join(format!("fno-x296f-retire-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        r#"
[harness.codex.attach]
kind = "unsupported"
tokens = []
"#,
    )
    .unwrap();
    let saved = std::env::var_os("FNO_GLOBAL_SETTINGS_PATH");
    std::env::set_var("FNO_GLOBAL_SETTINGS_PATH", dir.join("settings.yaml"));

    assert!(
        fno::agents_view::attach_form("codex").is_none(),
        "an explicit unsupported override is honored"
    );

    match saved {
        Some(v) => std::env::set_var("FNO_GLOBAL_SETTINGS_PATH", v),
        None => std::env::remove_var("FNO_GLOBAL_SETTINGS_PATH"),
    }
    std::fs::remove_dir_all(&dir).ok();
}
