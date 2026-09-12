//! Binary-level tests for the canonical `--schedule` flag and the bounded
//! `--daemon` compatibility alias.

use std::process::{Command, Output};
use tempfile::TempDir;

fn run_test_mode(flags: &[&str]) -> Output {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    let log_dir = temp.path().join("logs");
    Command::new(env!("CARGO_BIN_EXE_seakarr"))
        .args([
            "--config-path",
            config_dir.to_str().unwrap(),
            "--log-path",
            log_dir.to_str().unwrap(),
        ])
        .args(flags)
        .arg("--test")
        .output()
        .expect("failed to start seakarr")
}

fn combined(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn help_advertises_schedule_and_hides_daemon() {
    let output = Command::new(env!("CARGO_BIN_EXE_seakarr"))
        .arg("--help")
        .output()
        .expect("failed to start seakarr");
    let text = combined(&output);

    assert!(output.status.success(), "got:\n{text}");
    assert!(text.contains("--schedule"), "got:\n{text}");
    assert!(!text.contains("--daemon"), "got:\n{text}");
}

#[test]
fn schedule_is_accepted_without_deprecation_warning() {
    let output = run_test_mode(&["--schedule"]);
    let text = combined(&output);

    assert!(output.status.success(), "got:\n{text}");
    assert!(text.contains("Configuration is valid."), "got:\n{text}");
    assert!(!text.contains("deprecated"), "got:\n{text}");
}

#[test]
fn daemon_is_hidden_but_accepted_with_warning() {
    let output = run_test_mode(&["--daemon"]);
    let text = combined(&output);

    assert!(output.status.success(), "got:\n{text}");
    assert!(text.contains("Configuration is valid."), "got:\n{text}");
    assert!(
        text.contains("--daemon is deprecated; use --schedule"),
        "got:\n{text}"
    );
    assert!(text.contains("next minor release"), "got:\n{text}");
}

#[test]
fn both_schedule_flags_enable_once_and_warn_once() {
    let output = run_test_mode(&["--schedule", "--daemon"]);
    let text = combined(&output);

    assert!(output.status.success(), "got:\n{text}");
    assert_eq!(
        text.matches("--daemon is deprecated").count(),
        1,
        "got:\n{text}"
    );
}
