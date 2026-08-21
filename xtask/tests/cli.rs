//! Process-level coverage for the xtask command boundary.

use std::process::Command;

#[test]
fn fixture_command_propagates_success_and_failure_exit_codes() {
    let success = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["__perf-fixture", "--output-bytes", "4"])
        .output()
        .expect("run successful fixture command");
    assert!(success.status.success());
    assert_eq!(success.stdout, b"xxxx\n");

    let failure = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["__perf-fixture", "--exit-code", "7"])
        .output()
        .expect("run failing fixture command");
    assert_eq!(failure.status.code(), Some(7));
}

#[test]
fn invalid_perf_command_uses_the_invalid_campaign_exit_code() {
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "perf",
            "report",
            "--run-dir",
            "target/perf/runs/does-not-exist",
        ])
        .output()
        .expect("run invalid report command");

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("error:"));
}
