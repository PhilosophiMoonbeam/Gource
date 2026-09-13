// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Command-line parsing and configuration composition for the native app.
//!
//! Configuration is deliberately assembled in one place.  Defaults are
//! overridden by a TOML file, then `GOURCE_*` environment variables, and
//! finally explicit command-line values.  Every value reaches the core
//! validator before an input is opened or a window is created.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use clap::{ArgAction, Args, Parser, Subcommand};
use gource_core::{CameraMode, ConfigError as CoreConfigError, Rational, ReplayConfig};
use gource_ingest::{DEFAULT_CACHE_BYTES, IngestLimits, IngestOptions, InputSpec};
use serde::Deserialize;
use thiserror::Error;

/// The largest viewport accepted by the native presenter.
const MAX_VIEWPORT_EDGE: u32 = 32_768;

/// A validated physical viewport extent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

impl Viewport {
    pub const DEFAULT: Self = Self {
        width: 1280,
        height: 720,
    };

    pub fn new(width: u32, height: u32) -> Result<Self, ConfigError> {
        if width == 0 || height == 0 {
            return Err(ConfigError::InvalidViewport {
                value: format!("{width}x{height}"),
                reason: "width and height must both be non-zero",
            });
        }
        if width > MAX_VIEWPORT_EDGE || height > MAX_VIEWPORT_EDGE {
            return Err(ConfigError::InvalidViewport {
                value: format!("{width}x{height}"),
                reason: "an edge exceeds the native viewport limit",
            });
        }
        Ok(Self { width, height })
    }

    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        let value = value.trim();
        let (width, height) =
            value
                .split_once(['x', 'X', '×'])
                .ok_or_else(|| ConfigError::InvalidViewport {
                    value: value.to_owned(),
                    reason: "expected WIDTHxHEIGHT",
                })?;
        let width = width
            .parse::<u32>()
            .map_err(|_| ConfigError::InvalidViewport {
                value: value.to_owned(),
                reason: "width is not an unsigned integer",
            })?;
        let height = height
            .parse::<u32>()
            .map_err(|_| ConfigError::InvalidViewport {
                value: value.to_owned(),
                reason: "height is not an unsigned integer",
            })?;
        Self::new(width, height)
    }
}

impl Default for Viewport {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl FromStr for Viewport {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for Viewport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}x{}", self.width, self.height)
    }
}

/// An exact positive rational frame rate accepted by the export command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameRate {
    pub numerator: u32,
    pub denominator: u32,
}

impl FrameRate {
    pub const DEFAULT: Self = Self {
        numerator: 60,
        denominator: 1,
    };

    pub fn new(numerator: u32, denominator: u32) -> Result<Self, ConfigError> {
        if numerator == 0 || denominator == 0 {
            return Err(ConfigError::InvalidFrameRate {
                value: format!("{numerator}/{denominator}"),
                reason: "numerator and denominator must both be non-zero",
            });
        }
        let divisor = gcd_u32(numerator, denominator);
        Ok(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }

    pub fn as_f64(self) -> f64 {
        self.numerator as f64 / self.denominator as f64
    }
}

impl Default for FrameRate {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl FromStr for FrameRate {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if let Some((numerator, denominator)) = value.split_once('/') {
            let numerator =
                numerator
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| ConfigError::InvalidFrameRate {
                        value: value.to_owned(),
                        reason: "numerator is not an unsigned integer",
                    })?;
            let denominator =
                denominator
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| ConfigError::InvalidFrameRate {
                        value: value.to_owned(),
                        reason: "denominator is not an unsigned integer",
                    })?;
            return Self::new(numerator, denominator);
        }
        let spelling = value
            .parse::<f64>()
            .map_err(|_| ConfigError::InvalidFrameRate {
                value: value.to_owned(),
                reason: "expected FPS or NUMERATOR/DENOMINATOR",
            })?;
        if !spelling.is_finite() || spelling <= 0.0 || spelling > u32::MAX as f64 {
            return Err(ConfigError::InvalidFrameRate {
                value: value.to_owned(),
                reason: "frame rate must be finite and positive",
            });
        }
        let rational = Rational::from_f64(spelling).map_err(|_| ConfigError::InvalidFrameRate {
            value: value.to_owned(),
            reason: "frame rate cannot be represented exactly",
        })?;
        let numerator =
            u32::try_from(rational.numerator).map_err(|_| ConfigError::InvalidFrameRate {
                value: value.to_owned(),
                reason: "frame rate numerator is too large",
            })?;
        let denominator =
            u32::try_from(rational.denominator).map_err(|_| ConfigError::InvalidFrameRate {
                value: value.to_owned(),
                reason: "frame rate denominator is too large",
            })?;
        Self::new(numerator, denominator)
    }
}

impl fmt::Display for FrameRate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.denominator == 1 {
            write!(formatter, "{}", self.numerator)
        } else {
            write!(formatter, "{}/{}", self.numerator, self.denominator)
        }
    }
}

/// A rational repository time used by export configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepositoryTime(pub Rational);

impl FromStr for RepositoryTime {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let rational = if let Some((numerator, denominator)) = value.split_once('/') {
            let numerator = numerator
                .trim()
                .parse::<i128>()
                .map_err(|_| ConfigError::InvalidRepositoryTime(value.to_owned()))?;
            let denominator = denominator
                .trim()
                .parse::<i128>()
                .map_err(|_| ConfigError::InvalidRepositoryTime(value.to_owned()))?;
            Rational::new(numerator, denominator)
        } else {
            let spelling = value
                .parse::<f64>()
                .map_err(|_| ConfigError::InvalidRepositoryTime(value.to_owned()))?;
            Rational::from_f64(spelling)
        }
        .map_err(|_| ConfigError::InvalidRepositoryTime(value.to_owned()))?;
        if rational.numerator < 0 {
            return Err(ConfigError::InvalidRepositoryTime(value.to_owned()));
        }
        Ok(Self(rational))
    }
}
impl fmt::Display for RepositoryTime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.denominator == 1 {
            write!(formatter, "{}", self.0.numerator)
        } else {
            write!(formatter, "{}/{}", self.0.numerator, self.0.denominator)
        }
    }
}

impl Default for RepositoryTime {
    fn default() -> Self {
        Self(Rational::ZERO)
    }
}

/// The supported top-level commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandKind {
    View,
    Export,
    Diagnose,
}

/// Command-line parser for `gource-app`.
#[derive(Clone, Debug, Parser)]
#[command(
    name = "gource-app",
    version,
    about = "Deterministic repository history visualizer",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Optional TOML configuration file.
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub fn resolve(&self) -> Result<ResolvedCommand, ConfigError> {
        let config_path = self
            .config
            .clone()
            .or_else(|| std::env::var_os("GOURCE_CONFIG").map(PathBuf::from));
        let file = config_path
            .as_deref()
            .map(ConfigOverrides::from_path)
            .transpose()?;
        let environment = EnvironmentOverrides::from_env();
        compose_config(self, file.as_ref(), &environment)
    }

    pub fn command_kind(&self) -> CommandKind {
        match self.command {
            Command::View(_) => CommandKind::View,
            Command::Export(_) => CommandKind::Export,
            Command::Diagnose(_) => CommandKind::Diagnose,
        }
    }
}

/// The supported subcommands.  Legacy Gource flags are intentionally absent;
/// clap reports them as unknown arguments instead of silently approximating
/// their old behavior.
#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    /// Open an interactive native scene.
    View(ViewArgs),
    /// Render deterministic frames without creating a window.
    Export(ExportArgs),
    /// Measure serial and bounded-parallel replay without a GPU.
    Diagnose(DiagnoseArgs),
}

/// Options common to both commands.
#[derive(Clone, Debug, Args, Default)]
pub struct CommonArgs {
    /// Custom log path, or `-` for finite stdin.
    #[arg(long, value_name = "PATH")]
    pub input: Option<String>,
    /// Private directory for persistent indexed-history entries. Omitted to
    /// disable caching.
    #[arg(long = "cache-dir", value_name = "PATH")]
    pub cache_dir: Option<PathBuf>,
    /// Maximum aggregate bytes occupied by persistent cache entries.
    #[arg(long = "cache-bytes", value_name = "BYTES")]
    pub cache_bytes: Option<u64>,
    /// Physical viewport as WIDTHxHEIGHT.
    #[arg(long, value_name = "WIDTHxHEIGHT")]
    pub viewport: Option<Viewport>,
    /// Physical viewport width (must be paired with --height).
    #[arg(long, value_name = "PIXELS")]
    pub width: Option<u32>,
    /// Physical viewport height (must be paired with --width).
    #[arg(long, value_name = "PIXELS")]
    pub height: Option<u32>,
    /// Repository seconds represented by one simulated day.
    #[arg(long = "seconds-per-day", value_name = "SECONDS")]
    pub seconds_per_day: Option<f64>,
    /// Run at real-time repository speed.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_realtime")]
    pub realtime: bool,
    /// Explicitly disable real-time repository speed.
    #[arg(long = "no-realtime", action = ArgAction::SetTrue, conflicts_with = "realtime")]
    pub no_realtime: bool,
    /// Skip idle repository periods longer than this many seconds.
    #[arg(
        long = "auto-skip-seconds",
        alias = "auto-skip",
        value_name = "SECONDS"
    )]
    pub auto_skip_seconds: Option<f64>,
    /// Multiply the canonical playback speed.
    #[arg(long = "time-scale", value_name = "FACTOR")]
    pub time_scale: Option<f64>,
    /// Keep deleted files visible for this many seconds.
    #[arg(
        long = "file-idle-seconds",
        alias = "file-idle",
        value_name = "SECONDS"
    )]
    pub file_idle_seconds: Option<f64>,
    /// Automatic camera policy: overview or track.
    #[arg(long, value_name = "MODE")]
    pub camera: Option<String>,
    /// Deterministic layout seed.
    #[arg(long)]
    pub seed: Option<u64>,
    /// Path filter. Repeat to replace the configured filter set.
    #[arg(long = "filter", action = ArgAction::Append, value_name = "GLOB")]
    pub filters: Vec<String>,
    /// Maximum bytes in one physical record.
    #[arg(long = "max-record-bytes", value_name = "BYTES")]
    pub max_record_bytes: Option<u64>,
    /// Maximum finite input bytes.
    #[arg(long = "max-input-bytes", value_name = "BYTES")]
    pub max_input_bytes: Option<u64>,
    /// Maximum normalized path bytes.
    #[arg(long = "max-path-bytes", value_name = "BYTES")]
    pub max_path_bytes: Option<u64>,
    /// Maximum contributor bytes.
    #[arg(long = "max-contributor-bytes", value_name = "BYTES")]
    pub max_contributor_bytes: Option<u64>,
    /// Maximum path components.
    #[arg(long = "max-path-components", value_name = "COUNT")]
    pub max_path_components: Option<u64>,
    /// Maximum event count.
    #[arg(long = "max-events", value_name = "COUNT")]
    pub max_events: Option<u64>,
    /// Working memory limit shared by parser and index construction.
    #[arg(long = "working-memory-bytes", value_name = "BYTES")]
    pub working_memory_bytes: Option<u64>,
    /// Temporary sorting disk limit.
    #[arg(long = "working-disk-bytes", value_name = "BYTES")]
    pub working_disk_bytes: Option<u64>,
    /// Maximum merge fan-in.
    #[arg(long = "run-fan-in", value_name = "COUNT")]
    pub run_fan_in: Option<u64>,
}

/// Interactive scene command arguments.
#[derive(Clone, Debug, Args)]
pub struct ViewArgs {
    #[command(flatten)]
    pub common: CommonArgs,
}

/// Headless export command arguments.
#[derive(Clone, Debug, Args)]
pub struct ExportArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    /// Destination directory or file, depending on --video.
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,
    /// Export a video through the export runner's video backend.
    #[arg(long, action = ArgAction::SetTrue)]
    pub video: bool,
    /// Exact output frame rate (FPS or NUMERATOR/DENOMINATOR).
    #[arg(long = "frame-rate", alias = "fps", value_name = "RATE")]
    pub frame_rate: Option<FrameRate>,
    /// First repository time to export.
    #[arg(long, value_name = "SECONDS")]
    pub start: Option<RepositoryTime>,
    /// Last repository time to export.
    #[arg(long, value_name = "SECONDS")]
    pub end: Option<RepositoryTime>,
}

/// Headless replay measurement arguments.
#[derive(Clone, Debug, Args)]
pub struct DiagnoseArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    /// Number of Rayon workers for the bounded parallel replay.
    #[arg(long, value_name = "COUNT", value_parser = parse_nonzero_threads)]
    pub threads: Option<NonZeroUsize>,
    /// Last repository time to replay. `--target` is an explicit alias.
    #[arg(long, visible_alias = "target", value_name = "SECONDS")]
    pub end: Option<RepositoryTime>,
}

/// TOML configuration layer.  Every field is optional so precedence can be
/// tested without conflating an absent value with an explicit default.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ConfigOverrides {
    pub input: Option<String>,
    pub cache_dir: Option<PathBuf>,
    pub cache_bytes: Option<u64>,
    pub viewport: Option<ViewportOverride>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub seconds_per_day: Option<f64>,
    pub realtime: Option<bool>,
    pub auto_skip_seconds: Option<f64>,
    pub time_scale: Option<f64>,
    pub file_idle_seconds: Option<f64>,
    pub camera: Option<String>,
    pub seed: Option<u64>,
    pub filters: Option<Vec<String>>,
    pub max_record_bytes: Option<u64>,
    pub max_input_bytes: Option<u64>,
    pub max_path_bytes: Option<u64>,
    pub max_contributor_bytes: Option<u64>,
    pub max_path_components: Option<u64>,
    pub max_events: Option<u64>,
    pub working_memory_bytes: Option<u64>,
    pub working_disk_bytes: Option<u64>,
    pub run_fan_in: Option<u64>,
    pub threads: Option<usize>,
    pub output: Option<PathBuf>,
    pub video: Option<bool>,
    pub frame_rate: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub view: Option<CommandOverrides>,
    pub export: Option<CommandOverrides>,
    pub diagnose: Option<CommandOverrides>,
}

/// Optional command-specific TOML section.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CommandOverrides {
    pub input: Option<String>,
    pub cache_dir: Option<PathBuf>,
    pub cache_bytes: Option<u64>,
    pub viewport: Option<ViewportOverride>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub seconds_per_day: Option<f64>,
    pub realtime: Option<bool>,
    pub auto_skip_seconds: Option<f64>,
    pub time_scale: Option<f64>,
    pub file_idle_seconds: Option<f64>,
    pub camera: Option<String>,
    pub seed: Option<u64>,
    pub filters: Option<Vec<String>>,
    pub max_record_bytes: Option<u64>,
    pub max_input_bytes: Option<u64>,
    pub max_path_bytes: Option<u64>,
    pub max_contributor_bytes: Option<u64>,
    pub max_path_components: Option<u64>,
    pub max_events: Option<u64>,
    pub working_memory_bytes: Option<u64>,
    pub working_disk_bytes: Option<u64>,
    pub run_fan_in: Option<u64>,
    pub threads: Option<usize>,
    pub output: Option<PathBuf>,
    pub video: Option<bool>,
    pub frame_rate: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
}

/// A viewport represented in either TOML's `"WIDTHxHEIGHT"` shorthand or a
/// table with `width` and `height` members.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum ViewportOverride {
    Text(String),
    Dimensions {
        width: Option<u32>,
        height: Option<u32>,
    },
}

impl ConfigOverrides {
    pub fn from_path(path: &Path) -> Result<Self, ConfigError> {
        let bytes = fs::read(path).map_err(|source| ConfigError::ConfigIo {
            path: path.to_owned(),
            source,
        })?;
        toml::from_slice(&bytes).map_err(|source| ConfigError::ConfigParse {
            path: path.to_owned(),
            source,
        })
    }

    pub fn from_toml_str(value: &str) -> Result<Self, ConfigError> {
        toml::from_str(value).map_err(|source| ConfigError::ConfigTextParse { source })
    }
}

/// Environment layer used by [`compose_config`].
#[derive(Clone, Debug, Default)]
pub struct EnvironmentOverrides {
    values: BTreeMap<String, String>,
}

impl EnvironmentOverrides {
    pub fn from_env() -> Self {
        Self {
            values: std::env::vars().collect(),
        }
    }

    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            values: pairs
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
}

/// Fully composed and validated settings shared by view, export, and
/// diagnostics.
#[derive(Clone, Debug, PartialEq)]
pub struct AppConfig {
    pub command: CommandKind,
    pub input: InputSpec,
    /// Private persistent indexed-history cache directory. `None` disables
    /// cache creation and lookup.
    pub cache_dir: Option<PathBuf>,
    /// Maximum aggregate bytes occupied by cache entries.
    pub cache_bytes: u64,
    pub viewport: Viewport,
    pub replay: ReplayConfig,
    pub filters: Vec<String>,
    pub output: Option<PathBuf>,
    pub video: bool,
    pub frame_rate: FrameRate,
    pub start: RepositoryTime,
    pub end: Option<RepositoryTime>,
    pub run_fan_in: u64,
    /// Explicit worker count for diagnostics; `None` selects available
    /// parallelism at invocation time.
    pub threads: Option<NonZeroUsize>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            command: CommandKind::View,
            input: InputSpec::Stdin,
            cache_dir: None,
            cache_bytes: DEFAULT_CACHE_BYTES,
            viewport: Viewport::DEFAULT,
            replay: ReplayConfig::default(),
            filters: Vec::new(),
            output: None,
            video: false,
            frame_rate: FrameRate::DEFAULT,
            start: RepositoryTime::default(),
            end: None,
            run_fan_in: 32,
            threads: None,
        }
    }
}

impl AppConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.cache_bytes == 0 || self.cache_bytes == u64::MAX {
            return Err(ConfigError::InvalidCacheBytes(self.cache_bytes));
        }
        Viewport::new(self.viewport.width, self.viewport.height)?;
        self.replay.validate().map_err(ConfigError::ReplayConfig)?;
        if self.filters.iter().any(|filter| filter.trim().is_empty()) {
            return Err(ConfigError::EmptyFilter);
        }
        if self.threads.is_some_and(|threads| threads.get() == 0) {
            return Err(ConfigError::InvalidThreads(0));
        }
        if self.command == CommandKind::Export {
            if self.output.is_none() {
                return Err(ConfigError::MissingExportOutput);
            }
            if let Some(end) = self.end
                && end.0 < self.start.0
            {
                return Err(ConfigError::InvalidExportRange);
            }
        }
        Ok(())
    }

    pub fn ingest_limits(&self) -> IngestLimits {
        IngestLimits {
            max_record_bytes: self.replay.limits.record_bytes,
            max_input_bytes: self.replay.limits.input_bytes,
            max_path_bytes: self.replay.limits.path_bytes,
            max_contributor_bytes: self.replay.limits.contributor_bytes,
            max_path_components: self.replay.limits.path_components,
            max_events: self.replay.limits.max_events,
            working_memory_bytes: self.replay.limits.working_memory_bytes,
            run_fan_in: self.run_fan_in,
            working_disk_bytes: self.replay.limits.working_disk_bytes,
        }
    }

    pub fn ingest_options(&self) -> IngestOptions {
        IngestOptions::default().with_limits(self.ingest_limits())
    }
}

/// A command after configuration composition and validation.
#[derive(Clone, Debug, PartialEq)]
pub enum ResolvedCommand {
    View(AppConfig),
    Export(AppConfig),
    Diagnose(AppConfig),
}

impl ResolvedCommand {
    pub fn config(&self) -> &AppConfig {
        match self {
            Self::View(config) | Self::Export(config) | Self::Diagnose(config) => config,
        }
    }

    pub fn into_config(self) -> AppConfig {
        match self {
            Self::View(config) | Self::Export(config) | Self::Diagnose(config) => config,
        }
    }
}

/// Compose defaults, TOML, environment, and command-line values in that exact
/// order.  The environment argument is explicit to keep precedence tests
/// deterministic and avoid mutating process-global environment variables.
pub fn compose_config(
    cli: &Cli,
    file: Option<&ConfigOverrides>,
    environment: &EnvironmentOverrides,
) -> Result<ResolvedCommand, ConfigError> {
    let command = cli.command_kind();
    let mut layer = ConfigOverrides::default();
    if let Some(file) = file {
        layer.merge(file, command);
    }
    layer.merge_environment(environment)?;
    layer.merge_cli(cli);
    let config = layer.into_app_config(command)?;
    Ok(match command {
        CommandKind::View => ResolvedCommand::View(config),
        CommandKind::Export => ResolvedCommand::Export(config),
        CommandKind::Diagnose => ResolvedCommand::Diagnose(config),
    })
}

/// Alias with a descriptive name for callers that compose a command from a
/// parsed CLI value.
pub fn resolve_cli(
    cli: &Cli,
    file: Option<&ConfigOverrides>,
    environment: &EnvironmentOverrides,
) -> Result<ResolvedCommand, ConfigError> {
    compose_config(cli, file, environment)
}

impl ConfigOverrides {
    fn merge(&mut self, source: &Self, command: CommandKind) {
        self.merge_fields(source);
        let command_source = match command {
            CommandKind::View => source.view.as_ref(),
            CommandKind::Export => source.export.as_ref(),
            CommandKind::Diagnose => source.diagnose.as_ref(),
        };
        if let Some(command_source) = command_source {
            self.merge_command_fields(command_source);
        }
    }

    fn merge_environment(&mut self, environment: &EnvironmentOverrides) -> Result<(), ConfigError> {
        macro_rules! env_value {
            ($field:ident, $key:literal, $parse:expr) => {
                if let Some(value) = environment.get($key) {
                    self.$field =
                        Some(
                            $parse(value).map_err(|reason| ConfigError::InvalidEnvironment {
                                key: $key,
                                value: value.to_owned(),
                                reason,
                            })?,
                        );
                }
            };
        }
        if let Some(value) = environment.get("GOURCE_INPUT") {
            self.input = Some(value.to_owned());
        }
        if let Some(value) = environment.get("GOURCE_CACHE_DIR") {
            self.cache_dir = Some(PathBuf::from(value));
        }
        env_value!(cache_bytes, "GOURCE_CACHE_BYTES", parse_u64);
        let environment_viewport = environment
            .get("GOURCE_VIEWPORT")
            .map(|value| ViewportOverride::Text(value.to_owned()));
        let environment_width = environment
            .get("GOURCE_WIDTH")
            .map(|value| {
                parse_u32(value).map_err(|reason| ConfigError::InvalidEnvironment {
                    key: "GOURCE_WIDTH",
                    value: value.to_owned(),
                    reason,
                })
            })
            .transpose()?;
        let environment_height = environment
            .get("GOURCE_HEIGHT")
            .map(|value| {
                parse_u32(value).map_err(|reason| ConfigError::InvalidEnvironment {
                    key: "GOURCE_HEIGHT",
                    value: value.to_owned(),
                    reason,
                })
            })
            .transpose()?;
        self.merge_viewport_layer(environment_viewport, environment_width, environment_height);
        env_value!(seconds_per_day, "GOURCE_SECONDS_PER_DAY", parse_f64);
        env_value!(auto_skip_seconds, "GOURCE_AUTO_SKIP_SECONDS", parse_f64);
        env_value!(time_scale, "GOURCE_TIME_SCALE", parse_f64);
        env_value!(file_idle_seconds, "GOURCE_FILE_IDLE_SECONDS", parse_f64);
        env_value!(seed, "GOURCE_SEED", parse_u64);
        env_value!(max_record_bytes, "GOURCE_MAX_RECORD_BYTES", parse_u64);
        env_value!(max_input_bytes, "GOURCE_MAX_INPUT_BYTES", parse_u64);
        env_value!(max_path_bytes, "GOURCE_MAX_PATH_BYTES", parse_u64);
        env_value!(
            max_contributor_bytes,
            "GOURCE_MAX_CONTRIBUTOR_BYTES",
            parse_u64
        );
        env_value!(max_path_components, "GOURCE_MAX_PATH_COMPONENTS", parse_u64);
        env_value!(max_events, "GOURCE_MAX_EVENTS", parse_u64);
        env_value!(
            working_memory_bytes,
            "GOURCE_WORKING_MEMORY_BYTES",
            parse_u64
        );
        env_value!(working_disk_bytes, "GOURCE_WORKING_DISK_BYTES", parse_u64);
        env_value!(run_fan_in, "GOURCE_RUN_FAN_IN", parse_u64);
        env_value!(threads, "GOURCE_THREADS", parse_usize);
        if let Some(value) = environment.get("GOURCE_REALTIME") {
            self.realtime =
                Some(
                    parse_bool(value).map_err(|reason| ConfigError::InvalidEnvironment {
                        key: "GOURCE_REALTIME",
                        value: value.to_owned(),
                        reason,
                    })?,
                );
        }
        if let Some(value) = environment.get("GOURCE_CAMERA") {
            self.camera = Some(value.to_owned());
        }
        if let Some(value) = environment.get("GOURCE_FILTER") {
            self.filters = Some(split_list(value));
        }
        if let Some(value) = environment.get("GOURCE_OUTPUT") {
            self.output = Some(PathBuf::from(value));
        }
        if let Some(value) = environment.get("GOURCE_VIDEO") {
            self.video =
                Some(
                    parse_bool(value).map_err(|reason| ConfigError::InvalidEnvironment {
                        key: "GOURCE_VIDEO",
                        value: value.to_owned(),
                        reason,
                    })?,
                );
        }
        if let Some(value) = environment.get("GOURCE_FRAME_RATE") {
            self.frame_rate = Some(value.to_owned());
        }
        if let Some(value) = environment.get("GOURCE_START") {
            self.start = Some(value.to_owned());
        }
        if let Some(value) = environment.get("GOURCE_END") {
            self.end = Some(value.to_owned());
        }
        Ok(())
    }

    fn merge_cli(&mut self, cli: &Cli) {
        let common = match &cli.command {
            Command::View(args) => &args.common,
            Command::Export(args) => &args.common,
            Command::Diagnose(args) => &args.common,
        };
        self.input = common.input.clone().or_else(|| self.input.clone());
        self.cache_dir = common.cache_dir.clone().or_else(|| self.cache_dir.clone());
        self.cache_bytes = common.cache_bytes.or(self.cache_bytes);
        self.merge_viewport_layer(
            common
                .viewport
                .map(|viewport| ViewportOverride::Text(viewport.to_string())),
            common.width,
            common.height,
        );
        self.seconds_per_day = common.seconds_per_day.or(self.seconds_per_day);
        if common.realtime {
            self.realtime = Some(true);
        }
        if common.no_realtime {
            self.realtime = Some(false);
        }
        self.auto_skip_seconds = common.auto_skip_seconds.or(self.auto_skip_seconds);
        self.time_scale = common.time_scale.or(self.time_scale);
        self.file_idle_seconds = common.file_idle_seconds.or(self.file_idle_seconds);
        self.camera = common.camera.clone().or_else(|| self.camera.clone());
        self.seed = common.seed.or(self.seed);
        if !common.filters.is_empty() {
            self.filters = Some(common.filters.clone());
        }
        self.max_record_bytes = common.max_record_bytes.or(self.max_record_bytes);
        self.max_input_bytes = common.max_input_bytes.or(self.max_input_bytes);
        self.max_path_bytes = common.max_path_bytes.or(self.max_path_bytes);
        self.max_contributor_bytes = common.max_contributor_bytes.or(self.max_contributor_bytes);
        self.max_path_components = common.max_path_components.or(self.max_path_components);
        self.max_events = common.max_events.or(self.max_events);
        self.working_memory_bytes = common.working_memory_bytes.or(self.working_memory_bytes);
        self.working_disk_bytes = common.working_disk_bytes.or(self.working_disk_bytes);
        self.run_fan_in = common.run_fan_in.or(self.run_fan_in);
        match &cli.command {
            Command::Export(args) => {
                self.output = args.output.clone().or_else(|| self.output.clone());
                if args.video {
                    self.video = Some(true);
                }
                self.frame_rate = args
                    .frame_rate
                    .map(|rate| rate.to_string())
                    .or_else(|| self.frame_rate.clone());
                self.start = args
                    .start
                    .map(|time| time.to_string())
                    .or_else(|| self.start.clone());
                self.end = args
                    .end
                    .map(|time| time.to_string())
                    .or_else(|| self.end.clone());
            }
            Command::Diagnose(args) => {
                self.threads = args.threads.map(NonZeroUsize::get).or(self.threads);
                self.end = args
                    .end
                    .map(|time| time.to_string())
                    .or_else(|| self.end.clone());
            }
            Command::View(_) => {}
        }
    }

    fn merge_fields(&mut self, source: &Self) {
        macro_rules! copy {
            ($field:ident) => {
                if source.$field.is_some() {
                    self.$field = source.$field.clone();
                }
            };
        }
        copy!(input);
        copy!(cache_dir);
        copy!(cache_bytes);
        self.merge_viewport_layer(source.viewport.clone(), source.width, source.height);
        copy!(seconds_per_day);
        copy!(realtime);
        copy!(auto_skip_seconds);
        copy!(time_scale);
        copy!(file_idle_seconds);
        copy!(camera);
        copy!(seed);
        copy!(filters);
        copy!(max_record_bytes);
        copy!(max_input_bytes);
        copy!(max_path_bytes);
        copy!(max_contributor_bytes);
        copy!(max_path_components);
        copy!(max_events);
        copy!(working_memory_bytes);
        copy!(working_disk_bytes);
        copy!(run_fan_in);
        copy!(threads);
        copy!(output);
        copy!(video);
        copy!(frame_rate);
        copy!(start);
        copy!(end);
    }

    fn merge_command_fields(&mut self, source: &CommandOverrides) {
        macro_rules! copy {
            ($field:ident) => {
                if source.$field.is_some() {
                    self.$field = source.$field.clone();
                }
            };
        }
        copy!(input);
        copy!(cache_dir);
        copy!(cache_bytes);
        self.merge_viewport_layer(source.viewport.clone(), source.width, source.height);
        copy!(seconds_per_day);
        copy!(realtime);
        copy!(auto_skip_seconds);
        copy!(time_scale);
        copy!(file_idle_seconds);
        copy!(camera);
        copy!(seed);
        copy!(filters);
        copy!(max_record_bytes);
        copy!(max_input_bytes);
        copy!(max_path_bytes);
        copy!(max_contributor_bytes);
        copy!(max_path_components);
        copy!(max_events);
        copy!(working_memory_bytes);
        copy!(working_disk_bytes);
        copy!(run_fan_in);
        copy!(threads);
        copy!(output);
        copy!(video);
        copy!(frame_rate);
        copy!(start);
        copy!(end);
    }

    fn merge_viewport_layer(
        &mut self,
        shorthand: Option<ViewportOverride>,
        width: Option<u32>,
        height: Option<u32>,
    ) {
        if shorthand.is_some() {
            self.viewport = shorthand;
            self.width = width;
            self.height = height;
        } else if width.is_some() || height.is_some() {
            self.viewport = None;
            self.width = width;
            self.height = height;
        }
    }

    fn into_app_config(self, command: CommandKind) -> Result<AppConfig, ConfigError> {
        let viewport = resolve_viewport(self.viewport, self.width, self.height)?;
        let mut replay = ReplayConfig::default();
        let realtime = self.realtime.unwrap_or(false);
        if let Some(value) = self.seconds_per_day {
            replay.seconds_per_day = value;
        } else if realtime {
            // `--realtime` is a complete speed selection.  Fill in the
            // repository-day spelling expected by the shared validator while
            // retaining an explicitly supplied seconds-per-day conflict.
            replay.seconds_per_day = 86_400.0;
        }
        replay.realtime = realtime;
        if let Some(value) = self.auto_skip_seconds {
            replay.auto_skip_seconds = value;
        }
        if let Some(value) = self.time_scale {
            replay.time_scale = value;
        }
        if let Some(value) = self.file_idle_seconds {
            replay.file_idle_seconds = Some(value);
        }
        if let Some(value) = self.camera {
            replay.camera_mode =
                CameraMode::try_from(value.as_str()).map_err(ConfigError::ReplayConfig)?;
        }
        if let Some(value) = self.seed {
            replay.seed = value;
        }
        replay.limits.record_bytes = self.max_record_bytes.unwrap_or(replay.limits.record_bytes);
        replay.limits.input_bytes = self.max_input_bytes.unwrap_or(replay.limits.input_bytes);
        replay.limits.path_bytes = self.max_path_bytes.unwrap_or(replay.limits.path_bytes);
        replay.limits.contributor_bytes = self
            .max_contributor_bytes
            .unwrap_or(replay.limits.contributor_bytes);
        replay.limits.path_components = self
            .max_path_components
            .unwrap_or(replay.limits.path_components);
        replay.limits.max_events = self.max_events.unwrap_or(replay.limits.max_events);
        replay.limits.working_memory_bytes = self
            .working_memory_bytes
            .unwrap_or(replay.limits.working_memory_bytes);
        replay.limits.working_disk_bytes = self
            .working_disk_bytes
            .unwrap_or(replay.limits.working_disk_bytes);
        let frame_rate = self
            .frame_rate
            .as_deref()
            .map(FrameRate::from_str)
            .transpose()?;
        let start = self
            .start
            .as_deref()
            .map(RepositoryTime::from_str)
            .transpose()?
            .unwrap_or_default();
        let end = self
            .end
            .as_deref()
            .map(RepositoryTime::from_str)
            .transpose()?;
        let threads = self
            .threads
            .map(|value| {
                NonZeroUsize::new(value)
                    .map(Some)
                    .ok_or(ConfigError::InvalidThreads(value))
            })
            .transpose()?
            .flatten();
        let config = AppConfig {
            command,
            input: self
                .input
                .as_deref()
                .map(InputSpec::from)
                .unwrap_or(InputSpec::Stdin),
            cache_dir: self.cache_dir,
            cache_bytes: self.cache_bytes.unwrap_or(DEFAULT_CACHE_BYTES),
            viewport,
            replay,
            filters: self.filters.unwrap_or_default(),
            output: self.output,
            video: self.video.unwrap_or(false),
            frame_rate: frame_rate.unwrap_or_default(),
            start,
            end,
            run_fan_in: self.run_fan_in.unwrap_or(32),
            threads,
        };
        config.validate()?;
        Ok(config)
    }
}

fn resolve_viewport(
    shorthand: Option<ViewportOverride>,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<Viewport, ConfigError> {
    let explicit = match shorthand {
        Some(ViewportOverride::Text(value)) => Some(Viewport::parse(&value)?),
        Some(ViewportOverride::Dimensions {
            width: nested_width,
            height: nested_height,
        }) => Some(Viewport::new(
            nested_width.ok_or(ConfigError::IncompleteViewport)?,
            nested_height.ok_or(ConfigError::IncompleteViewport)?,
        )?),
        None => None,
    };
    if explicit.is_some() && (width.is_some() || height.is_some()) {
        return Err(ConfigError::ViewportConflict);
    }
    match explicit {
        Some(value) => Ok(value),
        None => match (width, height) {
            (Some(width), Some(height)) => Viewport::new(width, height),
            (None, None) => Ok(Viewport::default()),
            _ => Err(ConfigError::IncompleteViewport),
        },
    }
}

fn parse_u32(value: &str) -> Result<u32, &'static str> {
    value.parse().map_err(|_| "expected an unsigned integer")
}

fn parse_u64(value: &str) -> Result<u64, &'static str> {
    value.parse().map_err(|_| "expected an unsigned integer")
}
fn parse_usize(value: &str) -> Result<usize, &'static str> {
    value
        .parse()
        .map_err(|_| "expected a positive thread count")
}
fn parse_nonzero_threads(value: &str) -> Result<NonZeroUsize, &'static str> {
    value
        .parse()
        .map_err(|_| "thread count must be a non-zero unsigned integer")
}

fn parse_f64(value: &str) -> Result<f64, &'static str> {
    let value: f64 = value
        .parse()
        .map_err(|_| "expected a floating-point number")?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err("value must be finite")
    }
}

fn parse_bool(value: &str) -> Result<bool, &'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err("expected true/false"),
    }
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn gcd_u32(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left.max(1)
}

/// Configuration and input diagnostics raised before application startup.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid viewport '{value}': {reason}")]
    InvalidViewport { value: String, reason: &'static str },
    #[error("invalid frame rate '{value}': {reason}")]
    InvalidFrameRate { value: String, reason: &'static str },
    #[error("invalid repository time '{0}'")]
    InvalidRepositoryTime(String),
    #[error("viewport width and height must be supplied together")]
    IncompleteViewport,
    #[error("--viewport cannot be combined with --width or --height")]
    ViewportConflict,
    #[error("path filter cannot be empty")]
    EmptyFilter,
    #[error("thread count must be greater than zero (got {0})")]
    InvalidThreads(usize),
    #[error("cache byte capacity must be finite and greater than zero (got {0})")]
    InvalidCacheBytes(u64),
    #[error("export requires --output (and optionally --video)")]
    MissingExportOutput,
    #[error("export end time must not precede start time")]
    InvalidExportRange,
    #[error("cannot read config file {path}: {source}")]
    ConfigIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse config file {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("cannot parse config text: {source}")]
    ConfigTextParse { source: toml::de::Error },
    #[error("invalid environment variable {key}={value:?}: {reason}")]
    InvalidEnvironment {
        key: &'static str,
        value: String,
        reason: &'static str,
    },
    #[error("replay configuration rejected: {0}")]
    ReplayConfig(CoreConfigError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("CLI should parse")
    }

    #[test]
    fn packaged_command_name_is_stable() {
        let command = <Cli as clap::CommandFactory>::command();
        assert_eq!(command.get_name(), "gource-app");
    }

    #[test]
    fn precedence_is_defaults_then_file_then_environment_then_cli() {
        let parsed = cli(&["gource-app", "view", "--time-scale", "4", "--seed", "99"]);
        let file =
            ConfigOverrides::from_toml_str("time_scale = 2\nseed = 20\nviewport = \"800x600\"\n")
                .expect("TOML should parse");
        let environment = EnvironmentOverrides::from_pairs([
            ("GOURCE_TIME_SCALE", "3"),
            ("GOURCE_SEED", "30"),
            ("GOURCE_VIEWPORT", "1024x768"),
        ]);
        let resolved = compose_config(&parsed, Some(&file), &environment)
            .expect("composition should validate")
            .into_config();
        assert_eq!(resolved.replay.time_scale, 4.0);
        assert_eq!(resolved.replay.seed, 99);
        assert_eq!(resolved.viewport, Viewport::new(1024, 768).unwrap());
    }

    #[test]
    fn unsupported_legacy_flags_are_rejected_by_clap() {
        let result = Cli::try_parse_from(["gource-app", "view", "--git-log-command", "git log"]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_viewport_is_rejected_before_startup() {
        let result = Cli::try_parse_from(["gource-app", "view", "--viewport", "0x720"]);
        assert!(result.is_err());
    }

    #[test]
    fn command_specific_export_layer_does_not_leak_into_view() {
        let parsed = cli(&["gource-app", "view"]);
        let file = ConfigOverrides::from_toml_str(
            "[export]\noutput = \"frames\"\n[view]\ntime_scale = 2\n",
        )
        .expect("TOML should parse");
        let config = compose_config(&parsed, Some(&file), &EnvironmentOverrides::default())
            .expect("view config should validate")
            .into_config();
        assert_eq!(config.output, None);
        assert_eq!(config.replay.time_scale, 2.0);
    }

    #[test]
    fn realtime_is_complete_at_every_configuration_layer() {
        let cli_config = compose_config(
            &cli(&["gource-app", "view", "--realtime"]),
            None,
            &EnvironmentOverrides::default(),
        )
        .expect("CLI realtime should validate")
        .into_config();
        assert!(cli_config.replay.realtime);
        assert_eq!(cli_config.replay.seconds_per_day, 86_400.0);

        let environment_config = compose_config(
            &cli(&["gource-app", "view"]),
            None,
            &EnvironmentOverrides::from_pairs([("GOURCE_REALTIME", "true")]),
        )
        .expect("environment realtime should validate")
        .into_config();
        assert!(environment_config.replay.realtime);
        assert_eq!(environment_config.replay.seconds_per_day, 86_400.0);

        let file = ConfigOverrides::from_toml_str("realtime = true\n").expect("TOML should parse");
        let file_config = compose_config(
            &cli(&["gource-app", "view"]),
            Some(&file),
            &EnvironmentOverrides::default(),
        )
        .expect("TOML realtime should validate")
        .into_config();
        assert!(file_config.replay.realtime);
        assert_eq!(file_config.replay.seconds_per_day, 86_400.0);
    }

    #[test]
    fn explicit_seconds_per_day_keeps_realtime_conflict_visible() {
        let result = compose_config(
            &cli(&[
                "gource-app",
                "view",
                "--realtime",
                "--seconds-per-day",
                "10",
            ]),
            None,
            &EnvironmentOverrides::default(),
        );
        assert!(matches!(
            result,
            Err(ConfigError::ReplayConfig(CoreConfigError::RealtimeConflict))
        ));
    }

    #[test]
    fn higher_viewport_layer_replaces_the_other_form() {
        let dimensions = ConfigOverrides::from_toml_str("width = 800\nheight = 600\n")
            .expect("TOML should parse");
        let shorthand_wins = compose_config(
            &cli(&["gource-app", "view"]),
            Some(&dimensions),
            &EnvironmentOverrides::from_pairs([("GOURCE_VIEWPORT", "1024x768")]),
        )
        .expect("higher shorthand should replace lower dimensions")
        .into_config();
        assert_eq!(shorthand_wins.viewport, Viewport::new(1024, 768).unwrap());

        let shorthand =
            ConfigOverrides::from_toml_str("viewport = \"800x600\"").expect("TOML should parse");
        let dimensions_win = compose_config(
            &cli(&["gource-app", "view"]),
            Some(&shorthand),
            &EnvironmentOverrides::from_pairs([("GOURCE_WIDTH", "1024"), ("GOURCE_HEIGHT", "768")]),
        )
        .expect("higher dimensions should replace lower shorthand")
        .into_config();
        assert_eq!(dimensions_win.viewport, Viewport::new(1024, 768).unwrap());

        let incomplete = compose_config(
            &cli(&["gource-app", "view"]),
            Some(&shorthand),
            &EnvironmentOverrides::from_pairs([("GOURCE_WIDTH", "1024")]),
        );
        assert!(matches!(incomplete, Err(ConfigError::IncompleteViewport)));
    }

    #[test]
    fn cache_settings_follow_file_environment_and_cli_precedence() {
        let parsed = cli(&[
            "gource-app",
            "view",
            "--cache-dir",
            "cli-cache",
            "--cache-bytes",
            "300",
        ]);
        let file =
            ConfigOverrides::from_toml_str("cache_dir = \"file-cache\"\ncache_bytes = 100\n")
                .expect("TOML should parse");
        let environment = EnvironmentOverrides::from_pairs([
            ("GOURCE_CACHE_DIR", "environment-cache"),
            ("GOURCE_CACHE_BYTES", "200"),
        ]);

        let config = compose_config(&parsed, Some(&file), &environment)
            .expect("cache configuration should validate")
            .into_config();
        assert_eq!(config.cache_dir, Some(PathBuf::from("cli-cache")));
        assert_eq!(config.cache_bytes, 300);
    }

    #[test]
    fn cache_settings_are_available_in_each_command_section() {
        for (command, output) in [
            ("view", None),
            ("export", Some("frames")),
            ("diagnose", None),
        ] {
            let arguments = if let Some(output) = output {
                vec!["gource-app", command, "--output", output]
            } else {
                vec!["gource-app", command]
            };
            let parsed = cli(&arguments);
            let file = ConfigOverrides::from_toml_str(&format!(
                "[{command}]\ncache_dir = \"{command}-cache\"\ncache_bytes = 123\n"
            ))
            .expect("TOML should parse");
            let config = compose_config(&parsed, Some(&file), &EnvironmentOverrides::default())
                .expect("command cache configuration should validate")
                .into_config();
            assert_eq!(
                config.cache_dir,
                Some(PathBuf::from(format!("{command}-cache")))
            );
            assert_eq!(config.cache_bytes, 123);
        }
    }

    #[test]
    fn cache_byte_capacity_rejects_zero_and_unbounded_values() {
        for value in ["0", "18446744073709551615"] {
            let result = compose_config(
                &cli(&["gource-app", "view"]),
                None,
                &EnvironmentOverrides::from_pairs([("GOURCE_CACHE_BYTES", value)]),
            );
            assert!(matches!(result, Err(ConfigError::InvalidCacheBytes(_))));
        }
    }
}
