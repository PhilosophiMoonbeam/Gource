// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native Gource application shell.
//!
//! The app owns window/presentation state and translates user intent into
//! typed commands.  Parsing, replay, rendering, and export remain delegated to
//! their respective crates.

mod config;
mod diagnostics;
mod input;
mod native;

pub use config::{
    AppConfig, Cli, Command, CommandKind, CommonArgs, ConfigError, ConfigOverrides, DiagnoseArgs,
    EnvironmentOverrides, ExportArgs, FrameRate, RepositoryTime, ResolvedCommand, ViewArgs,
    Viewport, ViewportOverride, compose_config, resolve_cli,
};
pub use diagnostics::{DiagnosticsError, DiagnosticsReport, diagnose, run_diagnose};
pub use input::{AppCommand, CameraControlMode, CameraController, InputState, SelectionPoint};
pub use native::{
    FilteredHistory, NativeApplication, NativeError, load_history, run_export, run_view,
};
/// Errors raised while dispatching the selected top-level command.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Native(#[from] NativeError),
    #[error(transparent)]
    Diagnostics(#[from] DiagnosticsError),
}

/// Compatibility alias for callers that name the process-level error
/// `AppError`.
pub type AppError = RunError;

/// Parse and compose the process command line.
pub fn parse_cli() -> Result<ResolvedCommand, ConfigError> {
    let cli = <Cli as clap::Parser>::parse();
    cli.resolve()
}

/// Run the parsed command selected by the process command line.
pub fn run() -> Result<(), RunError> {
    match parse_cli()? {
        ResolvedCommand::View(config) => run_view(config).map_err(RunError::Native),
        ResolvedCommand::Export(config) => run_export(config).map(|_| ()).map_err(RunError::Native),
        ResolvedCommand::Diagnose(config) => run_diagnose(config).map_err(RunError::Diagnostics),
    }
}
