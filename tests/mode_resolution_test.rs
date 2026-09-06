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

#[test]
fn configured_auto_conflict_reports_absolute_config_path_and_line() {
    let cwd = std::env::current_dir().unwrap();
    let config_dir = cwd.join("target").join("provenance-conflict-test");
    let _ = std::fs::remove_dir_all(&config_dir);
    std::fs::create_dir_all(&config_dir).unwrap();
    let config_file = config_dir.join("seakarr.yml");
    let yaml = concat!(
        "# provenance test\n",
        "soulseek:\n",
        "  username: user\n",
        "  password: pass\n",
        "search:\n",
        "  default_mode: auto\n",
    );
    std::fs::write(&config_file, yaml).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_seakarr"))
        .args([
            "--config-path",
            "target/provenance-conflict-test",
            "--artist",
            "sleeper",
            "--album",
            "the modern age",
        ])
        .output()
        .expect("failed to start seakarr");

    // Capture the canonical path and the FINAL on-disk default_mode line
    // BEFORE cleanup: the binary reconciles the minimal fixture into the
    // full schema, so the reported line refers to the file as it exists
    // after the run, never a stale fixture line.
    let canonical = config_file.canonicalize().unwrap();
    let final_contents = std::fs::read_to_string(&config_file).unwrap();
    let mode_line = final_contents
        .lines()
        .position(|line| line.trim_start().starts_with("default_mode:"))
        .unwrap()
        + 1;

    let _ = std::fs::remove_dir_all(&config_dir);

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(1), "got:\n{combined}");
    assert!(
        combined.contains("search.default_mode: auto"),
        "got:\n{combined}"
    );
    assert!(
        combined.contains(&canonical.to_string_lossy().to_string()),
        "expected absolute path {canonical:?}, got:\n{combined}"
    );
    assert!(
        combined.contains(&format!(":{mode_line}")),
        "expected line {mode_line}, got:\n{combined}"
    );
    assert!(combined.contains("use --mode manual"), "got:\n{combined}");
    assert!(
        !combined.contains("Connecting to Soulseek"),
        "conflict must fail before login:\n{combined}"
    );
}

#[test]
fn test_mode_batch_with_batch_file_passes_validation() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    let log_dir = temp.path().join("logs");
    let batch_file = temp.path().join("wantlist.txt");
    std::fs::write(&batch_file, "Artist - Album\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_seakarr"))
        .args([
            "--config-path",
            config_dir.to_str().unwrap(),
            "--log-path",
            log_dir.to_str().unwrap(),
            "--mode",
            "batch",
            "--batch-file",
            batch_file.to_str().unwrap(),
            "--ignore-processed",
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
        "expected batch --test validation to pass, got:\n{combined}"
    );
    assert!(combined.contains("Configuration is valid."));
    assert!(!combined.contains("Connecting to Soulseek"));
}

#[test]
fn help_output_exposes_ignore_processed_flag() {
    // The --ignore-processed option must be advertised in the CLI help so
    // users can discover the reprocess override.
    let output = Command::new(env!("CARGO_BIN_EXE_seakarr"))
        .arg("--help")
        .output()
        .expect("failed to start seakarr");

    let help = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "--help must exit cleanly, got:\n{help}"
    );
    assert!(
        help.contains("--ignore-processed"),
        "--help must list --ignore-processed, got:\n{help}"
    );
    assert!(
        help.contains("processed-album record"),
        "--help must describe the processed-album override, got:\n{help}"
    );
}

#[test]
fn ignore_processed_with_manual_test_passes_validation() {
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
            "Artist",
            "--album",
            "Album",
            "--ignore-processed",
            "--test",
        ])
        .output()
        .expect("failed to start seakarr");

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(0), "got:\n{combined}");
    assert!(combined.contains("Configuration is valid."));
    assert!(!combined.contains("Connecting to Soulseek"));
}

#[test]
fn ignore_processed_with_daemon_fails_before_login() {
    // --ignore-processed cannot combine with --daemon: it would force a
    // reprocess on every daemon cycle. The validation must fire before any
    // Soulseek login or other startup side effects.
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    let log_dir = temp.path().join("logs");

    let output = Command::new(env!("CARGO_BIN_EXE_seakarr"))
        .args([
            "--config-path",
            config_dir.to_str().unwrap(),
            "--log-path",
            log_dir.to_str().unwrap(),
            "--ignore-processed",
            "--daemon",
            "--test",
            "--artist",
            "Artist",
            "--album",
            "Album",
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
        "--ignore-processed --daemon must fail validation, got:\n{combined}"
    );
    assert!(
        combined.contains("cannot be used with daemon mode"),
        "error must explain the daemon conflict, got:\n{combined}"
    );
    assert!(
        !combined.contains("Connecting to Soulseek"),
        "--ignore-processed --daemon must fail before login:\n{combined}"
    );
}
