use coport::config::Config;
use std::process::{Command, Output};

const BASE: &str = "listen_port: 8787\nrequest_timeout_seconds: 30\n";

fn run(config: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_coport"))
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap()
}

fn overrides(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

#[test]
fn repeated_short_overrides_are_validated_without_changing_source() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("config.yaml");
    std::fs::write(&source, BASE).unwrap();
    let config = Config::read_with_overrides(
        &source,
        &overrides(&[
            "listen_port=9000",
            "listen_port=9001",
            "request_timeout_seconds=12.5",
            "proxies.office=none",
            "codex.account_auth_file_only=false",
            "codex.routing.account_fallback=[office, none]",
            r"codex.routing.account.you@example\.com=office",
            "codex.homes=[]",
        ]),
    )
    .unwrap();
    assert_eq!(config.listen_port, 9001);
    assert_eq!(config.request_timeout_seconds, 12.5);
    assert!(!config.codex.account_auth_file_only);
    assert!(config.codex.homes.is_empty());
    assert_eq!(
        config.codex.routing.account["you@example.com"].label(),
        "office"
    );
    assert_eq!(
        config.codex.routing.account_fallback.unwrap().label(),
        "office, none"
    );
    assert_eq!(std::fs::read_to_string(&source).unwrap(), BASE);

    let output = run(
        &source,
        &[
            "-c",
            "listen_port=9001",
            "-c",
            "listen_port=9002",
            "--print-listen-port",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "9002");
    assert_eq!(std::fs::read_to_string(source).unwrap(), BASE);
}

#[test]
fn invalid_overrides_fail() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("config.yaml");
    std::fs::write(&source, BASE).unwrap();
    for value in [
        "listen_port",
        "=1",
        ".listen_port=1",
        "codex..homes=[]",
        "listen_port=0",
        "listen_port=oops",
        "listen_port.child=1",
        "request_timeout_seconds=4000",
        "codex.unknown=true",
        "codex.homes=[",
        r"codex.bad\escape=none",
        "codex.routing.account_fallback=missing",
    ] {
        let output = run(&source, &["-c", value, "--print-listen-port"]);
        assert!(!output.status.success(), "accepted {value}");
    }
    assert_eq!(std::fs::read_to_string(&source).unwrap(), BASE);
    assert!(
        !run(&source, &["--set", "listen_port=9000"])
            .status
            .success()
    );
    assert!(!run(&source, &["-c"]).status.success());
}

#[test]
fn values_can_contain_equals_and_check_uses_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("config.yaml");
    std::fs::write(&source, BASE).unwrap();
    let config = Config::read_with_overrides(
        &source,
        &overrides(&["proxies.office=http://user:pass=word@127.0.0.1:8080"]),
    )
    .unwrap();
    assert_eq!(
        config.proxies["office"],
        "http://user:pass=word@127.0.0.1:8080"
    );
    assert!(
        run(
            &source,
            &[
                "-c",
                "codex.homes=[]",
                "-c",
                "claude.config_dirs=[]",
                "--check"
            ]
        )
        .status
        .success()
    );
    assert!(
        !run(&source, &["-c", "listen_port=0", "--check"])
            .status
            .success()
    );
}
