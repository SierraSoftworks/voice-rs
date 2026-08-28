//! End-to-end CLI tests: run the real binary and assert on its exit code and
//! output streams.
//!
//! These exist chiefly as a regression guard for the silent-failure bug where
//! errors were routed through a tracing subscriber which the telemetry
//! session's disabled-in-debug default filtered out entirely: a failing
//! subcommand exited 1 with empty stdout *and* stderr. A failing command must
//! always explain itself on stderr.

use std::process::Command;

fn voice_orders() -> Command {
    Command::new(env!("CARGO_BIN_EXE_voice-orders"))
}

#[test]
fn test_a_failing_validate_explains_itself_on_stderr() {
    let output = voice_orders()
        .args(["validate", "/tmp/voice-orders-test-does-not-exist.yaml"])
        .output()
        .expect("the binary should run");

    assert_eq!(
        output.status.code(),
        Some(1),
        "a bad profile path is an error"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.trim().is_empty(),
        "a failing command must write its reason to stderr, but stderr was empty (stdout: {:?})",
        String::from_utf8_lossy(&output.stdout)
    );
    // The pretty renderer word-wraps long lines (splitting the path), so
    // assert on fragments which survive wrapping.
    assert!(
        stderr.contains("could not read the profile"),
        "the error should explain what failed, got: {stderr}"
    );
    assert!(
        stderr.contains("Advice"),
        "the error should carry human-errors advice, got: {stderr}"
    );
}

#[test]
fn test_a_refused_new_explains_itself_on_stderr() {
    let dir = tempfile::tempdir().expect("a temporary directory should be created");
    let existing = dir.path().join("profile.yaml");
    std::fs::write(&existing, "already here").expect("the file should be written");

    let output = voice_orders()
        .args(["new".as_ref(), existing.as_os_str()])
        .output()
        .expect("the binary should run");

    assert_eq!(
        output.status.code(),
        Some(1),
        "refusing to overwrite is an error"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.trim().is_empty(),
        "the overwrite refusal must reach stderr (stdout: {:?})",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("will not overwrite"),
        "the error should explain the refusal, got: {stderr}"
    );

    let content = std::fs::read_to_string(&existing).expect("the file should still exist");
    assert_eq!(
        content, "already here",
        "the existing file must be untouched"
    );
}
