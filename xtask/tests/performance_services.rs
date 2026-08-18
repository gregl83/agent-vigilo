//! Opt-in live PostgreSQL, RabbitMQ, and HTTP performance-harness tier.

use std::process::Command;

#[test]
fn isolated_topology_resets_collectors_and_cleans_exact_resources() {
    let status = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("__perf-service-fixture")
        .status()
        .expect("start performance service fixture");
    assert!(
        status.success(),
        "performance service fixture failed: {status}"
    );
}
