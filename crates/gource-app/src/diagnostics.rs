// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Headless replay diagnostics.
//!
//! Diagnostics deliberately use the same finite input loader and replay
//! session as the native/export paths.  Ingestion happens once; each measured
//! run receives an immutable clone of that indexed history.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Instant;

use gource_core::{HistorySource, ReplayConfig};
use gource_sim::{
    ExecutionMode, RationalTime, ReplayError, ReplaySession, SceneSnapshot, WorkBudget,
};
use thiserror::Error;

use crate::config::{AppConfig, CommandKind, ConfigError, RepositoryTime};
use crate::native::{NativeError, load_history};

/// Stable machine-readable measurements emitted by `gource-rs diagnose`.
#[derive(Clone, Debug, PartialEq)]
pub struct DiagnosticsReport {
    /// Finite input parsing and indexing duration in milliseconds.
    pub ingest_ms: f64,
    /// Serial replay duration in milliseconds.
    pub serial_ms: f64,
    /// Bounded parallel replay duration in milliseconds.
    pub parallel_ms: f64,
    /// Number of canonical events in the filtered indexed history.
    pub events: u64,
    /// Final serial replay tick (also the parallel tick on success).
    pub final_tick: u64,
    /// Whether final serial and parallel snapshots, including their ticks,
    /// compare equal.
    pub snapshots_equal: bool,
    /// Number of workers configured for the parallel run.
    pub configured_threads: usize,
    /// Final parallel replay tick, retained for a typed mismatch error.
    pub parallel_tick: u64,
}

impl DiagnosticsReport {
    /// Render one stable JSON object.  The field order is part of the CLI
    /// contract, and all values are finite primitive values.
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            "{{\"ingest_ms\":{:.3},\"serial_ms\":{:.3},\"parallel_ms\":{:.3},\"events\":{},\"final_tick\":{},\"snapshots_equal\":{},\"configured_threads\":{}}}",
            self.ingest_ms,
            self.serial_ms,
            self.parallel_ms,
            self.events,
            self.final_tick,
            self.snapshots_equal,
            self.configured_threads,
        )
    }
}

/// Errors raised by the headless diagnostic command.
#[derive(Debug, Error)]
pub enum DiagnosticsError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Native(#[from] NativeError),
    #[error(transparent)]
    Replay(#[from] ReplayError),
    #[error("diagnose command cannot run for {0:?} configuration")]
    WrongCommand(CommandKind),
    #[error("replay made no progress at tick {tick}")]
    Stalled { tick: u64 },
    #[error(
        "serial and parallel snapshots differ (serial_tick={serial_tick}, parallel_tick={parallel_tick})"
    )]
    SnapshotMismatch {
        serial_tick: u64,
        parallel_tick: u64,
    },
}

#[derive(Clone)]
struct ReplayOutcome {
    snapshot: Arc<SceneSnapshot>,
    tick: u64,
}

/// Parse/index once, then measure serial and bounded-parallel full replay.
///
/// If `config.end` is present, both runs seek to that repository timestamp.
/// Otherwise both runs advance until the finite history and all active action
/// lifetimes are complete.
pub fn diagnose(config: AppConfig) -> Result<DiagnosticsReport, DiagnosticsError> {
    if config.command != CommandKind::Diagnose {
        return Err(DiagnosticsError::WrongCommand(config.command));
    }
    config.validate()?;
    let ingest_started = Instant::now();
    let history = load_history(&config)?;
    let ingest_ms = elapsed_ms(ingest_started);
    let events = u64::try_from(history.len()).unwrap_or(u64::MAX);
    let threads = effective_threads(config.threads);

    // Clone before starting each timer so the reported replay measurements are
    // not dominated by duplicating the immutable indexed input.
    let serial_history = history.clone();
    let serial_started = Instant::now();
    let serial = replay(
        serial_history,
        &config.replay,
        ExecutionMode::Serial,
        config.end,
    )?;
    let serial_ms = elapsed_ms(serial_started);

    let parallel_history = history;
    let parallel_started = Instant::now();
    let parallel = replay(
        parallel_history,
        &config.replay,
        ExecutionMode::Parallel { threads },
        config.end,
    )?;
    let parallel_ms = elapsed_ms(parallel_started);

    let snapshots_equal = serial.tick == parallel.tick && serial.snapshot == parallel.snapshot;
    Ok(DiagnosticsReport {
        ingest_ms,
        serial_ms,
        parallel_ms,
        events,
        final_tick: serial.tick,
        snapshots_equal,
        configured_threads: threads.get(),
        parallel_tick: parallel.tick,
    })
}

/// Execute diagnostics and write the report to stdout.
///
/// The report is emitted even when the equality check fails.  The typed
/// mismatch error then makes the process exit non-zero through the app entry
/// point while preserving the machine-readable result on stdout.
pub fn run_diagnose(config: AppConfig) -> Result<(), DiagnosticsError> {
    let report = diagnose(config)?;
    println!("{}", report.to_json());
    if report.snapshots_equal {
        Ok(())
    } else {
        Err(DiagnosticsError::SnapshotMismatch {
            serial_tick: report.final_tick,
            parallel_tick: report.parallel_tick,
        })
    }
}

fn effective_threads(explicit: Option<NonZeroUsize>) -> NonZeroUsize {
    explicit.unwrap_or_else(|| {
        std::thread::available_parallelism().unwrap_or_else(|_| {
            // NonZeroUsize::new(1) is infallible; retaining this fallback keeps
            // diagnostics available on unusual restricted runtimes.
            NonZeroUsize::new(1).expect("one is non-zero")
        })
    })
}

fn replay<H: HistorySource>(
    history: H,
    config: &ReplayConfig,
    mode: ExecutionMode,
    end: Option<RepositoryTime>,
) -> Result<ReplayOutcome, DiagnosticsError> {
    let mut session = ReplaySession::new_with_execution(history, config.clone(), mode)?;
    if let Some(end) = end {
        session.seek_repository_time(RationalTime::from(end.0))?;
    } else {
        // The pump budget is finite, so a pathological event burst cannot
        // create an unbounded per-call allocation.  Repeated calls retain the
        // canonical in-progress tick until all due events are applied.
        while !session.is_at_end() {
            let tick = session.tick_id().get();
            let result = session.pump(WorkBudget::new(1, 4_096))?;
            if result.ticks == 0 && result.events == 0 && !result.complete {
                return Err(DiagnosticsError::Stalled { tick });
            }
        }
    }
    let tick = session.tick_id().get();
    Ok(ReplayOutcome {
        snapshot: session.snapshot(),
        tick,
    })
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1_000.0
}
