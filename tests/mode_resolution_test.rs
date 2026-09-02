use std::process::Command;
use tempfile::TempDir;

#[test]
fn test_mode_manual_with_artist_passes_validation() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    let log_dir = temp.path().join("logs");

    let output = Command::new(env!("CARGO_BIN_EXE_seakarr"))
        .args([
            "--config-path",
            config_dir.to_str().unwrap(),
            "--log-path",
            log_dir.to_str().unwrap(),
            "--mode",
            "manual",
            "--artist",
            "sleeper",
            "--test",
        ])
        .output()
        .expect("failed to start seakarr");

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "expected --test to accept a valid manual plan, got:\n{combined}"
    );
    assert!(
        combined.contains("Configuration is valid."),
        "valid --test invocation must report structural validation success:\n{combined}"
    );
    assert!(
        !combined.contains("Connecting to Soulseek"),
        "--test must not connect:\n{combined}"
    );
}

#[test]
fn test_mode_manual_without_target_fails_validation() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    let log_dir = temp.path().join("logs");

    let output = Command::new(env!("CARGO_BIN_EXE_seakarr"))
        .args([
            "--config-path",
            config_dir.to_str().unwrap(),
            "--log-path",
            log_dir.to_str().unwrap(),
            "--mode",
            "manual",
            "--test",
        ])
        .output()
        .expect("failed to start seakarr");

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected missing manual target to fail, got:\n{combined}"
    );
    assert!(
        combined.contains("at least one non-empty target"),
        "error must identify the missing manual target:\n{combined}"
    );
    assert!(
        !combined.contains("Connecting to Soulseek"),
        "mode errors must occur before login:\n{combined}"
    );
}

#[test]
fn artist_and_album_do_not_enter_configured_auto_mode() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    let log_dir = temp.path().join("logs");

    let output = Command::new(env!("CARGO_BIN_EXE_seakarr"))
        .args([
            "--config-path",
            config_dir.to_str().unwrap(),
            "--log-path",
            log_dir.to_str().unwrap(),
            "--listen-port",
            "2234",
            "--artist",
            "sleeper",
            "--album",
            "the modern age",
            "--test",
        ])
        .output()
        .expect("failed to start seakarr");

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected a mode-validation failure, got:\n{combined}"
    );
    assert!(
        combined.contains("--mode manual"),
        "error must tell the user how to select manual mode:\n{combined}"
    );
    assert!(
        !combined.contains("Connecting to Soulseek"),
        "mode errors must occur before login:\n{combined}"
    );
    assert!(
        !combined.contains("Scanning library"),
        "manual selectors must never enter the auto scanner:\n{combined}"
    );
}
