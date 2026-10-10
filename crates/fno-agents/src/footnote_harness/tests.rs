use super::*;

/// A Claude.ai login never reaches api.anthropic.com; a route env does. A
/// spawn whose footnote binary is missing refuses with the install command
/// and leaves no registry row. A spawn hands the binary a v2 spec that
/// carries the node and the permission mode.
#[test]
fn the_launcher_refuses_a_login_endpoint_and_a_missing_binary() {
    let cwd = Path::new("/");
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| pairs.iter().find(|p| p.0 == k).map(|p| p.1.to_string())
    };
    let err = endpoint::resolve_endpoint(
        cwd,
        &env(&[
            ("ANTHROPIC_BASE_URL", "https://api.anthropic.com"),
            ("ANTHROPIC_AUTH_TOKEN", "oauth"),
        ]),
    )
    .unwrap_err();
    assert!(err.contains("Claude Code lane"), "{err}");
    let ep = endpoint::resolve_endpoint(
        cwd,
        &env(&[
            ("ANTHROPIC_BASE_URL", "https://api.z.ai/api/anthropic"),
            ("ANTHROPIC_AUTH_TOKEN", "k"),
            ("FNO_ROUTE_PROVIDER", "zai"),
        ]),
    )
    .unwrap();
    assert_eq!(
        (ep.wire.as_str(), ep.provider_id.as_deref(), ep.bearer),
        ("anthropic", Some("zai"), true)
    );

    let tmp = tempfile::tempdir().unwrap();
    // The guard holds the crate's env lock for the rest of the test, so the
    // three launch vars below cannot race another env-reading test.
    let _home = crate::AgentsHomeEnvGuard::set(&tmp.path().join("agents-home"));
    let home = AgentsHome::from_env();
    let vars = [
        ("FNO_FOOTNOTE_BIN", "/nonexistent/footnote"),
        ("ANTHROPIC_BASE_URL", "https://api.z.ai/api/anthropic"),
        ("ANTHROPIC_AUTH_TOKEN", "k"),
    ];
    let saved: Vec<_> = vars
        .iter()
        .map(|(k, _)| (*k, std::env::var_os(k)))
        .collect();
    for (k, v) in vars {
        std::env::set_var(k, v);
    }
    let o = dispatch_once(
        &home,
        "fx-missing",
        "hi",
        "tester",
        tmp.path(),
        Some("glm-test"),
        None,
        &serde_json::json!({}),
    );
    let missing = o;
    // A stand-in binary keeps the spec it read and refuses, as a stale
    // binary would.
    let bin = tmp.path().join("footnote");
    let seen = tmp.path().join("spec.json");
    std::fs::write(
        &bin,
        format!("#!/bin/sh\ncat > '{}'\nexit 2\n", seen.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("FNO_FOOTNOTE_BIN", &bin);
    let refused = dispatch_once(
        &home,
        "fx-plan",
        "hi",
        "tester",
        tmp.path(),
        Some("glm-test"),
        None,
        &serde_json::json!({"node": "x-1", "permission_mode": "plan"}),
    );
    for (k, v) in saved {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }
    assert_eq!(missing.exit_code, 2, "{}", missing.stderr);
    assert!(
        missing
            .stderr
            .contains("cargo install --locked --path ~/code/footnote/fnh"),
        "{}",
        missing.stderr
    );
    assert_eq!(refused.exit_code, 2, "{}", refused.stderr);
    let spec: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&seen).unwrap()).unwrap();
    assert_eq!(spec["v"], 2);
    assert_eq!(spec["node"], "x-1");
    assert_eq!(spec["permission_mode"], "plan");
    let reg = load_registry(&home.registry_json()).unwrap();
    assert!(reg.find("fx-missing").is_none());
    assert!(reg.find("fx-plan").is_none());
}
