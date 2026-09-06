use crate::config::{CliOverrides, Config};
use crate::error::{Result, SeakarrError};

/// The three operations that seakarr can execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Auto,
    Manual,
    Batch,
}

/// Validated mode and the criteria needed by that mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionPlan {
    Auto,
    Manual {
        artist: Option<String>,
        album: Option<String>,
    },
    Batch {
        file_path: String,
    },
}

impl ExecutionPlan {
    /// Return the mode represented by this validated plan.
    pub fn mode(&self) -> SearchMode {
        match self {
            Self::Auto => SearchMode::Auto,
            Self::Manual { .. } => SearchMode::Manual,
            Self::Batch { .. } => SearchMode::Batch,
        }
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value.and_then(|value| {
        if value.trim().is_empty() {
            None
        } else {
            Some(value.to_owned())
        }
    })
}

fn configured_value(value: &str) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

fn non_empty_path(value: Option<&str>) -> Option<String> {
    non_empty(value).map(|value| value.trim().to_owned())
}

/// Format an auto-mode conflict error. When the mode came from the YAML config
/// (not `--mode`) and that config knows its on-disk source, point the user at
/// the exact `search.default_mode` line that caused the conflict. Otherwise
/// keep the concise legacy message (explicit `--mode` or in-memory config).
fn configured_mode_conflict(
    config: &Config,
    mode_from_cli: bool,
    selectors: &str,
    hint: &str,
) -> String {
    let (verb, selectors) = if selectors == "--batch-file" {
        ("is", selectors)
    } else {
        ("are", selectors)
    };
    let base = format!("{selectors} {verb} incompatible with auto mode");
    match (mode_from_cli, config.source()) {
        (false, Some(source)) => {
            // Omit the `:line` suffix when the line could not be located
            // (e.g. the key is absent) — never print a meaningless `:0`.
            let location = if source.default_mode_line > 0 {
                format!(" in {}:{}", source.path.display(), source.default_mode_line)
            } else {
                format!(" in {}", source.path.display())
            };
            format!(
                "{base}; search.default_mode: {}{location}; {hint}",
                config.search.default_mode.trim()
            )
        }
        _ => format!("{base}; {hint}"),
    }
}

/// Resolve and validate the operation selected by config and CLI overrides.
pub fn resolve_execution_plan(config: &Config, cli: &CliOverrides) -> Result<ExecutionPlan> {
    // --ignore-processed forces a reprocess on this run; combined with daemon
    // mode it would repeat the forced download every cycle, so reject the
    // combination before any mode resolution or startup side effects.
    if cli.ignore_processed && (cli.daemon || config.daemon.enabled) {
        return Err(SeakarrError::Config(
            "--ignore-processed cannot be used with daemon mode".into(),
        ));
    }

    let mode_from_cli = cli.mode.is_some();
    let raw_mode = cli
        .mode
        .as_deref()
        .unwrap_or(config.search.default_mode.as_str());
    let mode = match raw_mode.trim() {
        "auto" => SearchMode::Auto,
        "manual" => SearchMode::Manual,
        "batch" => SearchMode::Batch,
        value => {
            return Err(SeakarrError::Config(format!(
                "invalid search mode '{value}' (must be auto, manual, or batch)"
            )));
        }
    };

    let cli_artist = non_empty(cli.artist.as_deref());
    let cli_album = non_empty(cli.album.as_deref());
    let cli_batch_file = non_empty_path(cli.batch_file.as_deref());
    let has_manual_cli_selector = cli.artist.is_some() || cli.album.is_some();
    let has_batch_cli_selector = cli.batch_file.is_some();
    let has_blank_manual_cli_selector = [cli.artist.as_deref(), cli.album.as_deref()]
        .into_iter()
        .flatten()
        .any(|value| value.trim().is_empty());
    let has_blank_batch_cli_selector = cli
        .batch_file
        .as_deref()
        .is_some_and(|value| value.trim().is_empty());

    if has_manual_cli_selector && has_batch_cli_selector {
        return Err(SeakarrError::Config(
            "manual selectors --artist/--album cannot be combined with --batch-file".into(),
        ));
    }

    match mode {
        SearchMode::Auto => {
            if has_blank_batch_cli_selector {
                return Err(SeakarrError::Config(
                    "--batch-file must not be blank in auto mode; use --mode batch".into(),
                ));
            }
            if has_blank_manual_cli_selector {
                return Err(SeakarrError::Config(
                    "blank CLI selector is incompatible with auto mode; use --mode manual".into(),
                ));
            }
            if has_manual_cli_selector {
                return Err(SeakarrError::Config(configured_mode_conflict(
                    config,
                    mode_from_cli,
                    "--artist/--album",
                    "use --mode manual",
                )));
            }
            if has_batch_cli_selector {
                return Err(SeakarrError::Config(configured_mode_conflict(
                    config,
                    mode_from_cli,
                    "--batch-file",
                    "use --mode batch",
                )));
            }
            Ok(ExecutionPlan::Auto)
        }
        SearchMode::Manual => {
            if has_batch_cli_selector {
                return Err(SeakarrError::Config(
                    "--batch-file is incompatible with manual mode; use --mode batch".into(),
                ));
            }
            let artist = match cli.artist.as_deref() {
                Some(_) => cli_artist,
                None => configured_value(&config.search.manual.artist),
            };
            let album = match cli.album.as_deref() {
                Some(_) => cli_album,
                None => configured_value(&config.search.manual.album),
            };
            if artist.is_none() && album.is_none() {
                return Err(SeakarrError::Config(
                    "manual mode requires at least one non-empty target: --artist or ".to_owned()
                        + "--album (or search.manual values)",
                ));
            }
            Ok(ExecutionPlan::Manual { artist, album })
        }
        SearchMode::Batch => {
            if has_manual_cli_selector {
                return Err(SeakarrError::Config(
                    "--artist/--album are incompatible with batch mode; use --mode manual".into(),
                ));
            }
            let file_path = match cli.batch_file.as_deref() {
                Some(_) => cli_batch_file,
                None => non_empty_path(Some(&config.search.batch.file_path)),
            };
            let Some(file_path) = file_path else {
                return Err(SeakarrError::Config(
                    "batch mode requires --batch-file or search.batch.file_path".into(),
                ));
            };
            Ok(ExecutionPlan::Batch { file_path })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{configured_mode_conflict, resolve_execution_plan, ExecutionPlan, SearchMode};
    use crate::config::{CliOverrides, Config, ConfigSource};
    use crate::error::SeakarrError;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn config_with_mode(mode: &str) -> Config {
        let mut config = Config::default();
        config.search.default_mode = mode.to_owned();
        config
    }

    fn cli(
        mode: Option<&str>,
        artist: Option<&str>,
        album: Option<&str>,
        batch_file: Option<&str>,
    ) -> CliOverrides {
        CliOverrides {
            mode: mode.map(str::to_owned),
            artist: artist.map(str::to_owned),
            album: album.map(str::to_owned),
            batch_file: batch_file.map(str::to_owned),
            ..CliOverrides::default()
        }
    }

    fn assert_config_error(config: &Config, overrides: &CliOverrides, text: &str) {
        let error = resolve_execution_plan(config, overrides).expect_err("expected a config error");
        assert!(
            matches!(&error, &SeakarrError::Config(_)),
            "expected SeakarrError::Config, got {error:?}"
        );
        assert!(
            error.to_string().contains(text),
            "expected {text:?} in {error:?}"
        );
    }

    #[test]
    fn configured_auto_without_selectors_returns_auto() {
        let config = config_with_mode("auto");
        let plan = resolve_execution_plan(&config, &cli(None, None, None, None)).unwrap();
        assert_eq!(plan, ExecutionPlan::Auto);
        assert_eq!(plan.mode(), SearchMode::Auto);
    }

    #[test]
    fn configured_auto_manual_selector_error_identifies_yaml_source() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("seakarr.yml"),
            "search:\n  default_mode: auto\n",
        )
        .unwrap();
        let config = Config::load(dir.path()).unwrap();
        let error = resolve_execution_plan(&config, &cli(None, Some("Artist"), None, None))
            .unwrap_err()
            .to_string();
        let source = config.source().unwrap();
        assert!(error.contains("search.default_mode: auto"), "got: {error}");
        assert!(
            error.contains(&source.path.to_string_lossy().to_string()),
            "got: {error}"
        );
        assert!(
            error.contains(&format!(":{}", source.default_mode_line)),
            "got: {error}"
        );
        assert!(error.contains("use --mode manual"), "got: {error}");
    }

    #[test]
    fn configured_auto_batch_selector_error_identifies_yaml_source() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("seakarr.yml"),
            "search:\n  default_mode: auto\n",
        )
        .unwrap();
        let config = Config::load(dir.path()).unwrap();
        let error = resolve_execution_plan(&config, &cli(None, None, None, Some("wantlist.txt")))
            .unwrap_err()
            .to_string();
        let source = config.source().unwrap();
        assert!(error.contains("search.default_mode: auto"), "got: {error}");
        assert!(
            error.contains(&source.path.to_string_lossy().to_string()),
            "got: {error}"
        );
        assert!(
            error.contains(&format!(":{}", source.default_mode_line)),
            "got: {error}"
        );
        assert!(error.contains("use --mode batch"), "got: {error}");
    }

    #[test]
    fn configured_auto_rejects_manual_cli_selectors() {
        let config = config_with_mode("auto");
        assert_config_error(
            &config,
            &cli(None, None, Some("Album"), None),
            "--mode manual",
        );
    }

    #[test]
    fn configured_auto_rejects_batch_cli_selector() {
        let config = config_with_mode("auto");
        assert_config_error(
            &config,
            &cli(None, None, None, Some("wantlist.txt")),
            "--mode batch",
        );
    }

    #[test]
    fn auto_mode_rejects_blank_cli_selectors() {
        let config = config_with_mode("auto");
        for (artist, album) in [(Some("  "), None), (None, Some("\t"))] {
            assert_config_error(
                &config,
                &cli(None, artist, album, None),
                "blank CLI selector",
            );
        }
    }

    #[test]
    fn auto_mode_reports_blank_batch_file_specifically() {
        let config = config_with_mode("auto");
        assert_config_error(&config, &cli(None, None, None, Some("  ")), "--batch-file");
    }

    #[test]
    fn explicit_manual_mode_overrides_configured_auto() {
        let config = config_with_mode("auto");
        let plan = resolve_execution_plan(
            &config,
            &cli(Some("manual"), Some("Artist"), Some("Album"), None),
        )
        .unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Manual {
                artist: Some("Artist".into()),
                album: Some("Album".into()),
            }
        );
    }

    #[test]
    fn manual_mode_accepts_artist_only() {
        let config = config_with_mode("manual");
        let plan = resolve_execution_plan(&config, &cli(None, Some("Artist"), None, None)).unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Manual {
                artist: Some("Artist".into()),
                album: None,
            }
        );
    }

    #[test]
    fn manual_mode_accepts_album_only() {
        let config = config_with_mode("manual");
        let plan = resolve_execution_plan(&config, &cli(None, None, Some("Album"), None)).unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Manual {
                artist: None,
                album: Some("Album".into()),
            }
        );
    }

    #[test]
    fn manual_mode_uses_cli_values_before_config_values() {
        let mut config = config_with_mode("manual");
        config.search.manual.artist = "Configured Artist".into();
        config.search.manual.album = "Configured Album".into();
        let plan =
            resolve_execution_plan(&config, &cli(None, Some("CLI Artist"), None, None)).unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Manual {
                artist: Some("CLI Artist".into()),
                album: Some("Configured Album".into()),
            }
        );
    }

    #[test]
    fn manual_mode_accepts_explicit_blank_artist_as_album_only() {
        let mut config = config_with_mode("manual");
        config.search.manual.artist = "Configured Artist".into();
        let plan = resolve_execution_plan(
            &config,
            &cli(Some("manual"), Some("  "), Some("CLI Album"), None),
        )
        .unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Manual {
                artist: None,
                album: Some("CLI Album".into()),
            }
        );
    }

    #[test]
    fn cli_values_preserve_surrounding_whitespace() {
        let manual = config_with_mode("manual");
        let plan = resolve_execution_plan(
            &manual,
            &cli(Some("manual"), Some(" Artist "), Some(" Album "), None),
        )
        .unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Manual {
                artist: Some(" Artist ".into()),
                album: Some(" Album ".into()),
            }
        );

        let batch = config_with_mode("batch");
        let plan = resolve_execution_plan(
            &batch,
            &cli(Some("batch"), None, None, Some(" wantlist.txt ")),
        )
        .unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Batch {
                file_path: "wantlist.txt".into(),
            }
        );
    }

    #[test]
    fn batch_mode_rejects_explicit_blank_cli_path() {
        let mut config = config_with_mode("batch");
        config.search.batch.file_path = "configured.txt".into();
        assert_config_error(
            &config,
            &cli(Some("batch"), None, None, Some("  ")),
            "batch mode requires",
        );
    }

    #[test]
    fn manual_mode_requires_at_least_one_target() {
        let config = config_with_mode("manual");
        assert_config_error(
            &config,
            &cli(None, None, None, None),
            "at least one non-empty target",
        );
    }

    #[test]
    fn manual_mode_rejects_batch_file() {
        let config = config_with_mode("manual");
        assert_config_error(
            &config,
            &cli(None, None, None, Some("wantlist.txt")),
            "incompatible with manual mode",
        );
    }

    #[test]
    fn manual_mode_rejects_explicit_blank_batch_file() {
        let config = config_with_mode("manual");
        assert_config_error(
            &config,
            &cli(None, None, None, Some("  ")),
            "incompatible with manual mode",
        );
    }

    #[test]
    fn explicit_auto_rejects_manual_selector() {
        let config = config_with_mode("manual");
        assert_config_error(
            &config,
            &cli(Some("auto"), Some("Artist"), None, None),
            "--mode manual",
        );
    }

    #[test]
    fn explicit_auto_rejects_batch_selector() {
        let config = config_with_mode("manual");
        assert_config_error(
            &config,
            &cli(Some("auto"), None, None, Some("wantlist.txt")),
            "--mode batch",
        );
    }

    #[test]
    fn explicit_manual_rejects_batch_selector() {
        let config = config_with_mode("auto");
        assert_config_error(
            &config,
            &cli(Some("manual"), None, None, Some("wantlist.txt")),
            "incompatible with manual mode",
        );
    }

    #[test]
    fn explicit_batch_rejects_manual_selector() {
        let config = config_with_mode("auto");
        assert_config_error(
            &config,
            &cli(Some("batch"), Some("Artist"), None, None),
            "incompatible with batch mode",
        );
    }

    #[test]
    fn batch_mode_uses_cli_path_before_config_path() {
        let mut config = config_with_mode("batch");
        config.search.batch.file_path = "configured.txt".into();
        let plan =
            resolve_execution_plan(&config, &cli(None, None, None, Some("cli.txt"))).unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Batch {
                file_path: "cli.txt".into(),
            }
        );
    }

    #[test]
    fn batch_mode_uses_config_path_when_cli_path_is_absent() {
        let mut config = config_with_mode("batch");
        config.search.batch.file_path = "configured.txt".into();
        let plan = resolve_execution_plan(&config, &cli(None, None, None, None)).unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Batch {
                file_path: "configured.txt".into(),
            }
        );
    }

    #[test]
    fn batch_mode_requires_a_file() {
        let config = config_with_mode("batch");
        assert_config_error(&config, &cli(None, None, None, None), "batch mode requires");
    }

    #[test]
    fn batch_mode_rejects_manual_selectors() {
        let config = config_with_mode("batch");
        assert_config_error(
            &config,
            &cli(None, Some("Artist"), None, None),
            "incompatible with batch mode",
        );
    }

    #[test]
    fn manual_and_batch_cli_selectors_conflict() {
        let config = config_with_mode("auto");
        assert_config_error(
            &config,
            &cli(None, Some("Artist"), None, Some("wantlist.txt")),
            "cannot be combined",
        );
    }

    #[test]
    fn inactive_config_values_do_not_infer_mode() {
        let mut auto = config_with_mode("auto");
        auto.search.manual.artist = "Stale Artist".into();
        auto.search.batch.file_path = "stale.txt".into();
        let plan = resolve_execution_plan(&auto, &cli(None, None, None, None)).unwrap();
        assert_eq!(plan, ExecutionPlan::Auto);

        let mut manual = config_with_mode("manual");
        manual.search.manual.artist = "Artist".into();
        manual.search.batch.file_path = "stale.txt".into();
        let plan = resolve_execution_plan(&manual, &cli(None, None, None, None)).unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Manual {
                artist: Some("Artist".into()),
                album: None,
            }
        );

        let mut batch = config_with_mode("batch");
        batch.search.manual.artist = "Stale Artist".into();
        batch.search.batch.file_path = "wantlist.txt".into();
        let plan = resolve_execution_plan(&batch, &cli(None, None, None, None)).unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Batch {
                file_path: "wantlist.txt".into(),
            }
        );
    }

    #[test]
    fn whitespace_values_do_not_satisfy_manual_or_batch_requirements() {
        let mut manual = config_with_mode("manual");
        manual.search.manual.artist = "   ".into();
        manual.search.manual.album = "\t".into();
        assert_config_error(&manual, &cli(None, Some("  "), None, None), "at least one");

        let mut batch = config_with_mode("batch");
        batch.search.batch.file_path = "  ".into();
        assert_config_error(
            &batch,
            &cli(None, None, None, Some("\t")),
            "batch mode requires",
        );
    }

    #[test]
    fn unsupported_mode_is_rejected() {
        let config = config_with_mode("sideways");
        assert_config_error(
            &config,
            &cli(None, None, None, None),
            "must be auto, manual, or batch",
        );
    }

    #[test]
    fn empty_cli_mode_is_rejected() {
        // `--mode ""` trims to the empty string and must hit the invalid-mode
        // branch rather than being treated as "unset".
        let config = config_with_mode("auto");
        assert_config_error(
            &config,
            &cli(Some(""), None, None, None),
            "must be auto, manual, or batch",
        );
    }

    #[test]
    fn auto_mode_both_blank_selectors_report_blank_specific_message() {
        // Both selectors present-but-blank must report the blank-selector
        // message, not the generic manual/batch conflict.
        let config = config_with_mode("auto");
        assert_config_error(
            &config,
            &cli(None, Some("  "), Some("\t"), None),
            "blank CLI selector",
        );
    }

    #[test]
    fn empty_string_cli_values_behave_as_blank_in_manual_mode() {
        // `--artist ""` must be treated like any other blank value: the CLI
        // override still wins over a configured artist, leaving album-only.
        let mut config = config_with_mode("manual");
        config.search.manual.artist = "Configured Artist".into();
        let plan =
            resolve_execution_plan(&config, &cli(Some("manual"), Some(""), Some("Album"), None))
                .unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Manual {
                artist: None,
                album: Some("Album".into()),
            }
        );
    }

    #[test]
    fn configured_mode_conflict_omits_zero_line_suffix() {
        let mut config = config_with_mode("auto");
        config.source = Some(ConfigSource {
            path: PathBuf::from("/tmp/seakarr.yml"),
            default_mode_line: 0,
        });
        let message =
            configured_mode_conflict(&config, false, "--artist/--album", "use --mode manual");
        assert!(message.contains("in /tmp/seakarr.yml;"));
        assert!(!message.contains(":0"));
    }

    #[test]
    fn configured_batch_path_is_trimmed() {
        let mut config = config_with_mode("batch");
        config.search.batch.file_path = " configured.txt ".into();
        let plan = resolve_execution_plan(&config, &cli(None, None, None, None)).unwrap();
        assert_eq!(
            plan,
            ExecutionPlan::Batch {
                file_path: "configured.txt".into(),
            }
        );
    }

    #[test]
    fn ignore_processed_is_rejected_for_daemon_mode() {
        // --ignore-processed must never combine with daemon mode: it would
        // force a reprocess on every daemon cycle.
        let config = config_with_mode("auto");
        let mut overrides = cli(None, None, None, None);
        overrides.daemon = true;
        overrides.ignore_processed = true;

        assert_config_error(&config, &overrides, "cannot be used with daemon mode");
    }

    #[test]
    fn ignore_processed_is_rejected_for_configured_daemon() {
        // The daemon flag can come from the YAML config as well; both forms
        // must be rejected before any processing.
        let mut config = config_with_mode("auto");
        config.daemon.enabled = true;
        let mut overrides = cli(None, None, None, None);
        overrides.ignore_processed = true;

        assert_config_error(&config, &overrides, "cannot be used with daemon mode");
    }

    #[test]
    fn ignore_processed_is_allowed_for_one_shot_auto() {
        let config = config_with_mode("auto");
        let mut overrides = cli(None, None, None, None);
        overrides.ignore_processed = true;
        let plan = resolve_execution_plan(&config, &overrides).unwrap();
        assert_eq!(plan, ExecutionPlan::Auto);
    }
}
