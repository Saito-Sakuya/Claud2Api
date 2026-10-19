use std::process::Command;

#[test]
fn startup_rejects_unsafe_control_invalid_locale_and_missing_import() {
    for (name, value) in [
        ("CONTROL_BIND", "0.0.0.0:18082"),
        ("CLAUDE_LOCALE", "zh\r\nprivate-header: value"),
        ("CLAUDE_TIMEZONE", "Unknown/Timezone"),
        ("CLAUDE_COOKIE", "lastActiveOrg=fake-org"),
        (
            "STICKY_SESSION_STATE_FILE",
            "/missing-claud2api-import-fixture.json",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_claud2api"))
            .env_clear()
            .env(
                "GATEWAY_API_KEY",
                "fixture-config-gateway-key-32-characters-only",
            )
            .env(
                "ADMIN_API_KEY",
                "fixture-config-admin-key-32-characters-only",
            )
            .env(name, value)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("startup_failed"));
        assert!(!error.contains("fixture-config"));
    }
}
