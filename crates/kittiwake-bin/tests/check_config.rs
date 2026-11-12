//! `kittiwake --check-config`, which the install script runs before it swaps the binary.

use std::process::{Command, Output};

fn check(name: &str, toml: &str) -> Output {
    let dir = std::env::temp_dir().join(format!("kittiwake-check-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, toml).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_kittiwake"))
        .arg("--check-config")
        .env("KITTIWAKE_CONFIG", &path)
        // The MCP binary's variable, which the service's environment may well carry.
        .env("KITTIWAKE_API_URL", "https://api.example.test")
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();
    out
}

const GOOD: &str = "[nest]\norigin = \"https://nest.example\"\n";

#[test]
fn a_good_config_passes_and_stray_variables_only_warn() {
    let out = check("good", GOOD);
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("KITTIWAKE_API_URL"), "{stderr}");
}

#[test]
fn a_misplaced_key_is_refused_with_a_non_zero_exit() {
    let out = check(
        "bad",
        &format!("{GOOD}[limits]\nfoghorn_api_url = \"http://127.0.0.1:8081\"\n"),
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("limits.foghorn_api_url"), "{stderr}");
}
