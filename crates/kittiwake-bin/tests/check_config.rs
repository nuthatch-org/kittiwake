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

#[test]
fn variables_from_an_env_file_are_seen_by_the_check_and_nothing_is_executed() {
    let dir = std::env::temp_dir().join(format!("kittiwake-envfile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let canary = dir.join("canary");
    std::fs::write(&canary, "x").unwrap();
    let env = dir.join("env");
    // A bad key reaches the check only through the file, and the shell-looking line runs nothing.
    std::fs::write(
        &env,
        format!(
            "KITTIWAKE_NEST__ORIGIN=https://nest.example\nKITTIWAKE_BOGUS__KEY=a b;rm -f {}\nDB=a&b\n",
            canary.display()
        ),
    )
    .unwrap();
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, "bind = \"127.0.0.1:1\"\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_kittiwake"))
        .args(["--check-config", "--env-file"])
        .arg(&env)
        .env("KITTIWAKE_CONFIG", &cfg)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "nest.origin came from the file: {out:?}"
    );
    assert!(stderr.contains("KITTIWAKE_BOGUS"), "{stderr}");
    assert!(canary.exists(), "the file was executed");
    std::fs::remove_dir_all(&dir).ok();
}
