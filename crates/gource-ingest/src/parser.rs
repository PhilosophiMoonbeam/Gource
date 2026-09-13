// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Byte-oriented finite custom-log reader and normalizer.
//!
//! This module deliberately produces an intermediate representation.  The
//! public crate turns that representation into the immutable core catalog and
//! event index only after the complete source has been validated.

use std::cmp::Ordering as CmpOrdering;
use std::collections::BinaryHeap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tempfile::TempDir;

pub const DEFAULT_RECORD_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_INPUT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const DEFAULT_PATH_BYTES: u64 = 64 * 1024;
pub const DEFAULT_CONTRIBUTOR_BYTES: u64 = 4 * 1024;
pub const DEFAULT_PATH_COMPONENTS: u64 = 256;
pub const DEFAULT_WORKING_MEMORY_BYTES: u64 = 128 * 1024 * 1024;
pub const DEFAULT_WORKING_DISK_BYTES: u64 = 16 * 1024 * 1024 * 1024;
pub const DEFAULT_RUN_FAN_IN: u64 = 32;

const RUN_MAGIC: [u8; 8] = *b"GIRUN\0\0\x01";
const RUN_HEADER_BYTES: u64 = RUN_MAGIC.len() as u64;
const RECORD_FIXED_BYTES: u64 = 43;
/// The parser and final indexed-history builder use disjoint halves of the
/// configured working-memory budget.  Keeping this calculation here gives
/// both sides one canonical partition, including for odd limits.
pub(crate) const fn parser_working_memory_bytes(total: u64) -> u64 {
    total / 2
}

pub(crate) const fn index_working_memory_bytes(total: u64) -> u64 {
    total - parser_working_memory_bytes(total)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngestLimits {
    /// Maximum physical record size, including its optional LF delimiter.
    pub max_record_bytes: u64,
    /// Maximum finite source size, including BOM and line delimiters.
    pub max_input_bytes: u64,
    /// Maximum UTF-8 path byte length before lexical normalization.
    pub max_path_bytes: u64,
    /// Maximum normalized contributor byte length.
    pub max_contributor_bytes: u64,
    /// Maximum number of lexical path components.
    pub max_path_components: u64,
    /// Maximum number of normalized events in the indexed history.
    pub max_events: u64,
    /// Maximum bytes occupied by managed parser, merge, catalog, and index
    /// working state.  Published history storage is not reclaimed on behalf
    /// of the caller, but allocations made while constructing it are bounded.
    pub working_memory_bytes: u64,
    /// Maximum number of sorted runs opened by one bounded merge pass.
    pub run_fan_in: u64,
    /// Maximum aggregate bytes occupied by private temporary sort runs.
    pub working_disk_bytes: u64,
}

impl Default for IngestLimits {
    fn default() -> Self {
        Self {
            max_record_bytes: DEFAULT_RECORD_BYTES,
            max_input_bytes: DEFAULT_INPUT_BYTES,
            max_path_bytes: DEFAULT_PATH_BYTES,
            max_contributor_bytes: DEFAULT_CONTRIBUTOR_BYTES,
            max_path_components: DEFAULT_PATH_COMPONENTS,
            max_events: DEFAULT_INPUT_BYTES,
            working_memory_bytes: DEFAULT_WORKING_MEMORY_BYTES,
            run_fan_in: DEFAULT_RUN_FAN_IN,
            working_disk_bytes: DEFAULT_WORKING_DISK_BYTES,
        }
    }
}

impl IngestLimits {
    pub fn with_max_record_bytes(mut self, value: u64) -> Self {
        self.max_record_bytes = value;
        self
    }

    pub fn with_max_input_bytes(mut self, value: u64) -> Self {
        self.max_input_bytes = value;
        self
    }

    pub fn with_max_path_bytes(mut self, value: u64) -> Self {
        self.max_path_bytes = value;
        self
    }

    pub fn with_max_contributor_bytes(mut self, value: u64) -> Self {
        self.max_contributor_bytes = value;
        self
    }

    pub fn with_max_path_components(mut self, value: u64) -> Self {
        self.max_path_components = value;
        self
    }

    pub fn with_max_events(mut self, value: u64) -> Self {
        self.max_events = value;
        self
    }

    pub fn with_working_memory_bytes(mut self, value: u64) -> Self {
        self.working_memory_bytes = value;
        self
    }

    pub fn with_run_fan_in(mut self, value: u64) -> Self {
        self.run_fan_in = value;
        self
    }

    pub fn with_max_run_fan_in(self, value: u64) -> Self {
        self.with_run_fan_in(value)
    }

    pub fn with_working_disk_bytes(mut self, value: u64) -> Self {
        self.working_disk_bytes = value;
        self
    }

    pub(crate) fn validate(&self) -> Result<(), IngestError> {
        if self.max_record_bytes == 0 {
            return Err(limit_error(
                ErrorCode::RecordTooLarge,
                "max_record_bytes",
                self.max_record_bytes,
            ));
        }
        if self.max_input_bytes == 0 {
            return Err(limit_error(
                ErrorCode::InputTooLarge,
                "max_input_bytes",
                self.max_input_bytes,
            ));
        }
        if self.max_path_bytes == 0 {
            return Err(limit_error(
                ErrorCode::PathTooLong,
                "max_path_bytes",
                self.max_path_bytes,
            ));
        }
        if self.max_contributor_bytes == 0 {
            return Err(limit_error(
                ErrorCode::ContributorTooLong,
                "max_contributor_bytes",
                self.max_contributor_bytes,
            ));
        }
        if self.max_path_components == 0 {
            return Err(limit_error(
                ErrorCode::PathTooDeep,
                "max_path_components",
                self.max_path_components,
            ));
        }
        if self.max_events == 0 {
            return Err(limit_error(
                ErrorCode::EventCountLimit,
                "max_events",
                self.max_events,
            ));
        }
        if self.working_memory_bytes == 0 {
            return Err(limit_error(
                ErrorCode::WorkingMemoryLimit,
                "working_memory_bytes",
                self.working_memory_bytes,
            ));
        }
        if self.run_fan_in == 0 {
            return Err(limit_error(
                ErrorCode::RunFanInLimit,
                "run_fan_in",
                self.run_fan_in,
            ));
        }
        if self.working_disk_bytes == 0 {
            return Err(limit_error(
                ErrorCode::WorkingDiskLimit,
                "working_disk_bytes",
                self.working_disk_bytes,
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    Io,
    Cancelled,
    RecordTooLarge,
    InputTooLarge,
    EventCountLimit,
    WorkingMemoryLimit,
    RunFanInLimit,
    WorkingDiskLimit,
    WrongFieldCount,
    InvalidUtf8,
    InvalidTimestamp,
    InvalidAction,
    InvalidColour,
    InvalidPath,
    PathTooLong,
    ContributorTooLong,
    PathTooDeep,
    NulByte,
    LoneCarriageReturn,
    BomNotAtStart,
    DirectoryActionUnsupported,
}

impl ErrorCode {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Io => "io-error",
            Self::Cancelled => "cancelled",
            Self::RecordTooLarge => "record-too-large",
            Self::InputTooLarge => "input-too-large",
            Self::EventCountLimit => "event-count-limit",
            Self::WorkingMemoryLimit => "working-memory-limit",
            Self::RunFanInLimit => "run-fan-in-limit",
            Self::WorkingDiskLimit => "working-disk-limit",
            Self::WrongFieldCount => "wrong-field-count",
            Self::InvalidUtf8 => "invalid-utf8",
            Self::InvalidTimestamp => "invalid-timestamp",
            Self::InvalidAction => "invalid-action",
            Self::InvalidColour => "invalid-colour",
            Self::InvalidPath => "invalid-path",
            Self::PathTooLong => "path-too-long",
            Self::ContributorTooLong => "contributor-too-long",
            Self::PathTooDeep => "path-too-deep",
            Self::NulByte => "nul-byte",
            Self::LoneCarriageReturn => "lone-carriage-return",
            Self::BomNotAtStart => "bom-not-at-start",
            Self::DirectoryActionUnsupported => "directory-action-unsupported",
        }
    }
}

fn limit_error(code: ErrorCode, resource: &'static str, maximum: u64) -> IngestError {
    IngestError::new(
        code,
        0,
        0,
        "",
        format!("{resource} configured bound is {maximum}"),
    )
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngestError {
    pub code: ErrorCode,
    /// One-based physical line. Zero means that no physical record exists
    /// (for example, an OS error before the first read).
    pub line: u64,
    /// Zero-based byte offset of the physical record or failing source byte.
    pub byte_offset: u64,
    /// Bounded escaped context suitable for diagnostics.
    pub context: String,
    pub message: String,
}

impl IngestError {
    pub fn new(
        code: ErrorCode,
        line: u64,
        byte_offset: u64,
        context: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            line,
            byte_offset,
            context: context.into(),
            message: message.into(),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code.as_str()
    }

    pub fn is_cancelled(&self) -> bool {
        self.code == ErrorCode::Cancelled
    }
}

impl fmt::Display for IngestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code)?;
        if self.line != 0 {
            write!(f, " at line {} byte {}", self.line, self.byte_offset)?;
        } else {
            write!(f, " at byte {}", self.byte_offset)?;
        }
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        if !self.context.is_empty() {
            write!(f, " (context: {})", self.context)?;
        }
        Ok(())
    }
}

impl std::error::Error for IngestError {}

impl From<io::Error> for IngestError {
    fn from(value: io::Error) -> Self {
        Self::new(ErrorCode::Io, 0, 0, "", value.to_string())
    }
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressPhase {
    Reading,
    Normalizing,
    Sorting,
    Publishing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgressUpdate {
    pub phase: ProgressPhase,
    pub bytes_read: u64,
    pub input_bytes: Option<u64>,
    pub records_read: u64,
}

pub trait ProgressSink: Send + Sync {
    fn report(&self, update: ProgressUpdate);
}

impl<F> ProgressSink for F
where
    F: Fn(ProgressUpdate) + Send + Sync,
{
    fn report(&self, update: ProgressUpdate) {
        self(update);
    }
}

#[derive(Clone, Default)]
pub struct IngestOptions {
    pub limits: IngestLimits,
    pub cancellation: Option<CancellationToken>,
    pub progress: Option<Arc<dyn ProgressSink>>,
}

impl fmt::Debug for IngestOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IngestOptions")
            .field("limits", &self.limits)
            .field("cancellation", &self.cancellation.is_some())
            .field("progress", &self.progress.is_some())
            .finish()
    }
}

impl IngestOptions {
    pub fn with_limits(mut self, limits: IngestLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.cancellation = Some(token);
        self
    }

    pub fn with_progress<P>(mut self, sink: P) -> Self
    where
        P: ProgressSink + 'static,
    {
        self.progress = Some(Arc::new(sink));
        self
    }

    #[inline]
    pub(crate) fn check_cancelled(&self, line: u64, offset: u64) -> Result<(), IngestError> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            Err(IngestError::new(
                ErrorCode::Cancelled,
                line,
                offset,
                "",
                "input cancelled",
            ))
        } else {
            Ok(())
        }
    }

    #[inline]
    pub(crate) fn report(&self, update: ProgressUpdate) {
        if let Some(progress) = &self.progress {
            progress.report(update);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParsedAction {
    Add,
    Modify,
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedColour(pub [u8; 3]);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedRecord {
    pub timestamp: i64,
    pub username: String,
    pub action: ParsedAction,
    /// Normalized lexical path, without its optional virtual-root slash or
    /// directory marker.  `is_directory` retains the target kind.
    pub path: String,
    pub is_directory: bool,
    pub colour: Option<ParsedColour>,
    pub source_sequence: u64,
    pub line: u64,
    pub byte_offset: u64,
}

impl NormalizedRecord {
    pub(crate) fn estimated_bytes(&self) -> u64 {
        (size_of::<Self>() as u64)
            .saturating_add(self.username.capacity() as u64)
            .saturating_add(self.path.capacity() as u64)
    }
}

#[derive(Clone, Debug)]
pub struct ParsedInput {
    pub records: Vec<NormalizedRecord>,
    pub input_bytes: u64,
    pub input_hash: [u8; 32],
    pub(crate) record_count: u64,
    pub(crate) runs: Option<ParsedRuns>,
}

#[derive(Clone, Debug)]
pub(crate) struct ParsedRuns {
    pub(crate) tempdir: Arc<TempDir>,
    pub(crate) runs: Vec<SortRun>,
    pub(crate) disk_bytes: u64,
    pub(crate) next_run_id: u64,
}
fn parser_memory_limit(options: &IngestOptions) -> u64 {
    parser_working_memory_bytes(options.limits.working_memory_bytes)
}

fn parser_buffer_bytes(parser_limit: u64) -> u64 {
    if parser_limit == 0 {
        return 0;
    }
    // Keep a small bounded read-ahead buffer while leaving almost all of the
    // parser share available for records and sort state.
    parser_limit.min(8192).saturating_add(15) / 16
}

fn parser_payload_bytes(parser_limit: u64) -> u64 {
    parser_limit.saturating_sub(parser_buffer_bytes(parser_limit))
}

fn vec_bytes<T>(capacity: usize) -> u64 {
    u64::try_from(capacity)
        .ok()
        .and_then(|capacity| capacity.checked_mul(size_of::<T>() as u64))
        .unwrap_or(u64::MAX)
}

fn record_string_bytes(record: &NormalizedRecord) -> u64 {
    (record.username.capacity() as u64).saturating_add(record.path.capacity() as u64)
}

fn records_memory_bytes(records: &Vec<NormalizedRecord>) -> u64 {
    vec_bytes::<NormalizedRecord>(records.capacity()).saturating_add(
        records.iter().fold(0u64, |total, record| {
            total.saturating_add(record_string_bytes(record))
        }),
    )
}

fn sort_runs_memory_bytes(runs: &[SortRun], capacity: usize) -> u64 {
    runs.iter()
        .fold(vec_bytes::<SortRun>(capacity), |total, run| {
            total.saturating_add(run.path.as_os_str().len() as u64)
        })
}

fn parsed_runs_memory_bytes(state: &ParsedRuns) -> u64 {
    (size_of::<ParsedRuns>() as u64)
        .saturating_add(size_of::<TempDir>() as u64)
        .saturating_add(sort_runs_memory_bytes(&state.runs, state.runs.capacity()))
        .saturating_add(state.tempdir.path().as_os_str().len() as u64)
}

fn option_runs_memory_bytes(runs: &Option<ParsedRuns>) -> u64 {
    runs.as_ref().map_or(0, parsed_runs_memory_bytes)
}

fn parser_memory_check(
    requested: u64,
    options: &IngestOptions,
    line: u64,
    byte_offset: u64,
) -> Result<(), IngestError> {
    let maximum = parser_memory_limit(options);
    if requested > maximum {
        Err(working_memory_error(requested, maximum, line, byte_offset))
    } else {
        Ok(())
    }
}
fn parsed_runs_memory_with_path(state: &ParsedRuns, path: &Path, additional_slots: usize) -> u64 {
    (size_of::<ParsedRuns>() as u64)
        .saturating_add(size_of::<TempDir>() as u64)
        .saturating_add(sort_runs_memory_bytes(
            &state.runs,
            state.runs.capacity().saturating_add(additional_slots),
        ))
        .saturating_add(state.tempdir.path().as_os_str().len() as u64)
        .saturating_add(path.as_os_str().len() as u64)
}

#[derive(Clone, Debug)]
pub(crate) struct SortRun {
    pub(crate) path: PathBuf,
    pub(crate) records: u64,
    pub(crate) bytes: u64,
}

struct PhysicalRecord {
    bytes: Vec<u8>,
    terminated: bool,
    line: u64,
    byte_offset: u64,
}

pub(crate) fn parse_reader_with_callback<R: Read, F>(
    reader: R,
    options: &IngestOptions,
    mut on_record: F,
) -> Result<ParsedInput, IngestError>
where
    F: FnMut(&NormalizedRecord, u64) -> Result<(), IngestError>,
{
    options.limits.validate()?;
    let parser_limit = parser_memory_limit(options);
    let buffer_bytes = parser_buffer_bytes(parser_limit);
    let payload_limit = parser_payload_bytes(parser_limit);
    if payload_limit == 0 {
        return Err(working_memory_error(1, parser_limit, 0, 0));
    }
    let buffer_capacity = usize::try_from(buffer_bytes).unwrap_or(1).max(1);
    let mut input = BufReader::with_capacity(buffer_capacity, reader);
    let mut records = Vec::new();
    let mut chunk_bytes = 0u64;
    let mut line = 1u64;
    let mut byte_offset = 0u64;
    let mut first_record = true;
    let mut input_hash = blake3::Hasher::new();
    let mut saw_any_bytes = false;
    let mut input_bytes = 0u64;
    let mut record_count = 0u64;
    let mut runs = None;

    loop {
        options.check_cancelled(line, byte_offset)?;

        // Leave enough room for a worst-case physical record and its two
        // normalized strings.  This lets a full chunk spill before the next
        // read allocates through the parser share.
        if !records.is_empty() {
            let anticipated_body = options.limits.max_record_bytes.min(payload_limit);
            let anticipated = anticipated_body
                .checked_mul(2)
                .and_then(|value| value.checked_add(size_of::<NormalizedRecord>() as u64))
                .unwrap_or(u64::MAX);
            let requested = buffer_bytes
                .saturating_add(option_runs_memory_bytes(&runs))
                .saturating_add(chunk_bytes)
                .saturating_add(anticipated);
            if requested > parser_limit {
                spill_records(&mut records, &mut chunk_bytes, &mut runs, options)?;
            }
        }

        let Some(physical) = read_physical_record(
            &mut input,
            options,
            line,
            byte_offset,
            &mut input_hash,
            &mut input_bytes,
        )?
        else {
            break;
        };
        saw_any_bytes = true;
        let mut body = physical.bytes.as_slice();
        if first_record && body.starts_with(&[0xef, 0xbb, 0xbf]) {
            body = &body[3..];
            // A BOM followed immediately by EOF is the empty stream marker.
            if body.is_empty() && !physical.terminated {
                break;
            }
        }
        if body.windows(3).any(|window| window == [0xef, 0xbb, 0xbf]) {
            return Err(IngestError::new(
                ErrorCode::BomNotAtStart,
                physical.line,
                physical.byte_offset,
                bounded_context(body),
                "UTF-8 BOM is permitted only at stream start",
            ));
        }
        if body.contains(&0) {
            return Err(IngestError::new(
                ErrorCode::NulByte,
                physical.line,
                physical.byte_offset,
                bounded_context(body),
                "NUL is not valid in a custom-log record",
            ));
        }
        if body.contains(&b'\r') {
            return Err(IngestError::new(
                ErrorCode::LoneCarriageReturn,
                physical.line,
                physical.byte_offset,
                bounded_context(body),
                "carriage return is valid only as part of CRLF",
            ));
        }
        if body.is_empty() {
            return Err(IngestError::new(
                ErrorCode::WrongFieldCount,
                physical.line,
                physical.byte_offset,
                "",
                "blank records are not valid custom-log input",
            ));
        }

        // Parsing copies the username and path out of the physical record.
        // Include the physical allocation and the read-ahead buffer in the
        // transient parser charge before making those copies.
        let physical_bytes = vec_bytes::<u8>(physical.bytes.capacity());
        let body_bytes = body.len() as u64;
        let parse_bytes = physical_bytes
            .checked_add(body_bytes)
            .and_then(|value| value.checked_add(size_of::<NormalizedRecord>() as u64))
            .unwrap_or(u64::MAX);
        let mut requested = buffer_bytes
            .saturating_add(option_runs_memory_bytes(&runs))
            .saturating_add(chunk_bytes)
            .saturating_add(parse_bytes);
        if requested > parser_limit && !records.is_empty() {
            spill_records(&mut records, &mut chunk_bytes, &mut runs, options)?;
            requested = buffer_bytes
                .saturating_add(option_runs_memory_bytes(&runs))
                .saturating_add(chunk_bytes)
                .saturating_add(parse_bytes);
        }
        parser_memory_check(requested, options, physical.line, physical.byte_offset)?;

        let mut parsed = parse_record(body, physical.line, physical.byte_offset, &options.limits)?;
        if record_count >= options.limits.max_events {
            return Err(IngestError::new(
                ErrorCode::EventCountLimit,
                physical.line,
                physical.byte_offset,
                bounded_context(body),
                "configured event-count limit exceeded",
            ));
        }
        parsed.source_sequence = record_count;
        drop(physical);

        let record_bytes = parsed.estimated_bytes();
        let mut required = buffer_bytes
            .saturating_add(option_runs_memory_bytes(&runs))
            .saturating_add(chunk_bytes)
            .saturating_add(record_bytes)
            .saturating_add(if records.len() == records.capacity() {
                size_of::<NormalizedRecord>() as u64
            } else {
                0
            });
        if required > parser_limit && !records.is_empty() {
            spill_records(&mut records, &mut chunk_bytes, &mut runs, options)?;
            required = buffer_bytes
                .saturating_add(option_runs_memory_bytes(&runs))
                .saturating_add(chunk_bytes)
                .saturating_add(record_bytes)
                .saturating_add(if records.len() == records.capacity() {
                    size_of::<NormalizedRecord>() as u64
                } else {
                    0
                });
        }
        parser_memory_check(required, options, parsed.line, parsed.byte_offset)?;

        let old_capacity = records.capacity();
        if records.len() == old_capacity {
            records.try_reserve_exact(1).map_err(|_| {
                working_memory_error(
                    required.saturating_add(size_of::<NormalizedRecord>() as u64),
                    parser_limit,
                    parsed.line,
                    parsed.byte_offset,
                )
            })?;
        }
        let capacity_bytes = vec_bytes::<NormalizedRecord>(records.capacity())
            .saturating_sub(vec_bytes::<NormalizedRecord>(old_capacity));
        let working_with_record = buffer_bytes
            .saturating_add(option_runs_memory_bytes(&runs))
            .saturating_add(chunk_bytes)
            .saturating_add(record_bytes)
            .saturating_add(capacity_bytes);
        parser_memory_check(
            working_with_record,
            options,
            parsed.line,
            parsed.byte_offset,
        )?;
        on_record(&parsed, working_with_record)?;

        records.push(parsed);
        chunk_bytes = records_memory_bytes(&records);
        parser_memory_check(
            buffer_bytes
                .saturating_add(option_runs_memory_bytes(&runs))
                .saturating_add(chunk_bytes),
            options,
            line,
            byte_offset,
        )?;
        record_count = record_count.checked_add(1).ok_or_else(|| {
            IngestError::new(
                ErrorCode::EventCountLimit,
                line,
                byte_offset,
                "",
                "record sequence overflows u64",
            )
        })?;
        line = line.checked_add(1).ok_or_else(|| {
            IngestError::new(
                ErrorCode::InputTooLarge,
                line,
                byte_offset,
                "",
                "physical line number overflows u64",
            )
        })?;
        byte_offset = input_bytes;
        first_record = false;
        options.report(ProgressUpdate {
            phase: ProgressPhase::Reading,
            bytes_read: input_bytes,
            input_bytes: Some(input_bytes),
            records_read: record_count,
        });
    }

    if !saw_any_bytes {
        input_bytes = 0;
    }
    if !records.is_empty() && runs.is_some() {
        spill_records(&mut records, &mut chunk_bytes, &mut runs, options)?;
    }
    let input_hash = *input_hash.finalize().as_bytes();
    Ok(ParsedInput {
        records,
        input_bytes,
        input_hash,
        record_count,
        runs,
    })
}

fn working_memory_error(requested: u64, maximum: u64, line: u64, byte_offset: u64) -> IngestError {
    IngestError::new(
        ErrorCode::WorkingMemoryLimit,
        line,
        byte_offset,
        "",
        format!(
            "working_memory_bytes requested {requested} bytes exceeds configured bound {maximum}"
        ),
    )
}

fn working_disk_error(requested: u64, maximum: u64, line: u64, byte_offset: u64) -> IngestError {
    IngestError::new(
        ErrorCode::WorkingDiskLimit,
        line,
        byte_offset,
        "",
        format!(
            "working_disk_bytes requested {requested} bytes exceeds configured bound {maximum}"
        ),
    )
}

fn io_error(error: io::Error, line: u64, byte_offset: u64) -> IngestError {
    IngestError::new(ErrorCode::Io, line, byte_offset, "", error.to_string())
}

fn record_disk_bytes(record: &NormalizedRecord) -> Result<u64, IngestError> {
    let username = u64::try_from(record.username.len())
        .map_err(|_| working_disk_error(u64::MAX, u64::MAX - 1, record.line, record.byte_offset))?;
    let path = u64::try_from(record.path.len())
        .map_err(|_| working_disk_error(u64::MAX, u64::MAX - 1, record.line, record.byte_offset))?;
    let color = if record.colour.is_some() { 3 } else { 0 };
    RECORD_FIXED_BYTES
        .checked_add(username)
        .and_then(|value| value.checked_add(path))
        .and_then(|value| value.checked_add(color))
        .ok_or_else(|| working_disk_error(u64::MAX, u64::MAX - 1, record.line, record.byte_offset))
}

fn write_u64(file: &mut File, value: u64, line: u64, byte_offset: u64) -> Result<(), IngestError> {
    file.write_all(&value.to_le_bytes())
        .map_err(|error| io_error(error, line, byte_offset))
}

fn write_u32(file: &mut File, value: u32, line: u64, byte_offset: u64) -> Result<(), IngestError> {
    file.write_all(&value.to_le_bytes())
        .map_err(|error| io_error(error, line, byte_offset))
}

fn write_run(
    records: Vec<NormalizedRecord>,
    state: &mut ParsedRuns,
    options: &IngestOptions,
) -> Result<(), IngestError> {
    let run_id = state.next_run_id;
    state.next_run_id = state
        .next_run_id
        .checked_add(1)
        .ok_or_else(|| working_disk_error(u64::MAX, options.limits.working_disk_bytes, 0, 0))?;
    let path = state
        .tempdir
        .path()
        .join(format!("run-v1-{run_id:016x}.bin"));
    let additional_slots = usize::from(state.runs.len() == state.runs.capacity());
    let metadata_with_run = parsed_runs_memory_with_path(state, &path, additional_slots);
    parser_memory_check(
        metadata_with_run.saturating_add(records_memory_bytes(&records)),
        options,
        0,
        0,
    )?;
    if additional_slots != 0 {
        state.runs.try_reserve_exact(1).map_err(|_| {
            working_memory_error(metadata_with_run, parser_memory_limit(options), 0, 0)
        })?;
        parser_memory_check(
            parsed_runs_memory_with_path(state, &path, 0)
                .saturating_add(records_memory_bytes(&records)),
            options,
            0,
            0,
        )?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| io_error(error, 0, 0))?;
    let run_start = state.disk_bytes;
    let mut total = run_start
        .checked_add(RUN_HEADER_BYTES)
        .ok_or_else(|| working_disk_error(u64::MAX, options.limits.working_disk_bytes, 0, 0))?;
    if total > options.limits.working_disk_bytes {
        return Err(working_disk_error(
            total,
            options.limits.working_disk_bytes,
            0,
            0,
        ));
    }
    file.write_all(&RUN_MAGIC)
        .map_err(|error| io_error(error, 0, 0))?;
    let records_len = u64::try_from(records.len())
        .map_err(|_| working_disk_error(u64::MAX, options.limits.working_disk_bytes, 0, 0))?;
    for record in &records {
        options.check_cancelled(record.line, record.byte_offset)?;
        let encoded = record_disk_bytes(record)?;
        let next_total = total.checked_add(encoded).ok_or_else(|| {
            working_disk_error(
                u64::MAX,
                options.limits.working_disk_bytes,
                record.line,
                record.byte_offset,
            )
        })?;
        if next_total > options.limits.working_disk_bytes {
            return Err(working_disk_error(
                next_total,
                options.limits.working_disk_bytes,
                record.line,
                record.byte_offset,
            ));
        }
        write_u64(
            &mut file,
            record.timestamp as u64,
            record.line,
            record.byte_offset,
        )?;
        write_u64(
            &mut file,
            record.source_sequence,
            record.line,
            record.byte_offset,
        )?;
        write_u64(&mut file, record.line, record.line, record.byte_offset)?;
        write_u64(
            &mut file,
            record.byte_offset,
            record.line,
            record.byte_offset,
        )?;
        write_u32(
            &mut file,
            u32::try_from(record.username.len()).map_err(|_| {
                working_disk_error(
                    next_total,
                    options.limits.working_disk_bytes,
                    record.line,
                    record.byte_offset,
                )
            })?,
            record.line,
            record.byte_offset,
        )?;
        write_u32(
            &mut file,
            u32::try_from(record.path.len()).map_err(|_| {
                working_disk_error(
                    next_total,
                    options.limits.working_disk_bytes,
                    record.line,
                    record.byte_offset,
                )
            })?,
            record.line,
            record.byte_offset,
        )?;
        file.write_all(&[match record.action {
            ParsedAction::Add => b'A',
            ParsedAction::Modify => b'M',
            ParsedAction::Delete => b'D',
        }])
        .map_err(|error| io_error(error, record.line, record.byte_offset))?;
        file.write_all(&[u8::from(record.is_directory)])
            .map_err(|error| io_error(error, record.line, record.byte_offset))?;
        match record.colour {
            Some(colour) => {
                file.write_all(&[1, colour.0[0], colour.0[1], colour.0[2]])
                    .map_err(|error| io_error(error, record.line, record.byte_offset))?;
            }
            None => file
                .write_all(&[0])
                .map_err(|error| io_error(error, record.line, record.byte_offset))?,
        }
        file.write_all(record.username.as_bytes())
            .map_err(|error| io_error(error, record.line, record.byte_offset))?;
        file.write_all(record.path.as_bytes())
            .map_err(|error| io_error(error, record.line, record.byte_offset))?;
        total = next_total;
    }
    file.flush().map_err(|error| io_error(error, 0, 0))?;
    let run_bytes = total
        .checked_sub(run_start)
        .ok_or_else(|| working_disk_error(u64::MAX, options.limits.working_disk_bytes, 0, 0))?;
    state.disk_bytes = total;
    state.runs.push(SortRun {
        path,
        records: records_len,
        bytes: run_bytes,
    });
    Ok(())
}

fn spill_records(
    records: &mut Vec<NormalizedRecord>,
    chunk_bytes: &mut u64,
    runs: &mut Option<ParsedRuns>,
    options: &IngestOptions,
) -> Result<(), IngestError> {
    if records.is_empty() {
        *chunk_bytes = 0;
        return Ok(());
    }
    let owned = std::mem::take(records);
    *chunk_bytes = 0;
    let mut owned = owned;
    owned.sort_unstable_by_key(|record| (record.timestamp, record.source_sequence));
    let state = if let Some(state) = runs.as_mut() {
        state
    } else {
        parser_memory_check(
            (size_of::<ParsedRuns>() + size_of::<TempDir>()) as u64,
            options,
            0,
            0,
        )?;
        let tempdir = TempDir::new().map_err(|error| io_error(error, 0, 0))?;
        runs.insert(ParsedRuns {
            tempdir: Arc::new(tempdir),
            runs: Vec::new(),
            disk_bytes: 0,
            next_run_id: 0,
        })
    };
    parser_memory_check(
        parsed_runs_memory_bytes(state).saturating_add(records_memory_bytes(&owned)),
        options,
        0,
        0,
    )?;
    write_run(owned, state, options)
}

fn read_u64(file: &mut File, line: u64, byte_offset: u64) -> Result<u64, IngestError> {
    let mut bytes = [0u8; 8];
    file.read_exact(&mut bytes)
        .map_err(|error| io_error(error, line, byte_offset))?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_u32(file: &mut File, line: u64, byte_offset: u64) -> Result<u32, IngestError> {
    let mut bytes = [0u8; 4];
    file.read_exact(&mut bytes)
        .map_err(|error| io_error(error, line, byte_offset))?;
    Ok(u32::from_le_bytes(bytes))
}

fn write_normalized_record(file: &mut File, record: &NormalizedRecord) -> Result<(), IngestError> {
    write_u64(
        file,
        record.timestamp as u64,
        record.line,
        record.byte_offset,
    )?;
    write_u64(
        file,
        record.source_sequence,
        record.line,
        record.byte_offset,
    )?;
    write_u64(file, record.line, record.line, record.byte_offset)?;
    write_u64(file, record.byte_offset, record.line, record.byte_offset)?;
    write_u32(
        file,
        u32::try_from(record.username.len()).map_err(|_| {
            working_disk_error(u64::MAX, u64::MAX - 1, record.line, record.byte_offset)
        })?,
        record.line,
        record.byte_offset,
    )?;
    write_u32(
        file,
        u32::try_from(record.path.len()).map_err(|_| {
            working_disk_error(u64::MAX, u64::MAX - 1, record.line, record.byte_offset)
        })?,
        record.line,
        record.byte_offset,
    )?;
    file.write_all(&[match record.action {
        ParsedAction::Add => b'A',
        ParsedAction::Modify => b'M',
        ParsedAction::Delete => b'D',
    }])
    .map_err(|error| io_error(error, record.line, record.byte_offset))?;
    file.write_all(&[u8::from(record.is_directory)])
        .map_err(|error| io_error(error, record.line, record.byte_offset))?;
    match record.colour {
        Some(colour) => file
            .write_all(&[1, colour.0[0], colour.0[1], colour.0[2]])
            .map_err(|error| io_error(error, record.line, record.byte_offset))?,
        None => file
            .write_all(&[0])
            .map_err(|error| io_error(error, record.line, record.byte_offset))?,
    }
    file.write_all(record.username.as_bytes())
        .map_err(|error| io_error(error, record.line, record.byte_offset))?;
    file.write_all(record.path.as_bytes())
        .map_err(|error| io_error(error, record.line, record.byte_offset))?;
    Ok(())
}

struct RunReader {
    file: File,
    remaining: u64,
}

impl RunReader {
    fn open(run: &SortRun) -> Result<Self, IngestError> {
        let mut file = File::open(&run.path).map_err(|error| io_error(error, 0, 0))?;
        let mut magic = [0u8; RUN_MAGIC.len()];
        file.read_exact(&mut magic)
            .map_err(|error| io_error(error, 0, 0))?;
        if magic != RUN_MAGIC {
            return Err(IngestError::new(
                ErrorCode::Io,
                0,
                0,
                "",
                "temporary sort run has an unsupported version",
            ));
        }
        Ok(Self {
            file,
            remaining: run.records,
        })
    }

    fn next_record(
        &mut self,
        options: &IngestOptions,
        memory: &mut MergeMemory,
    ) -> Result<Option<NormalizedRecord>, IngestError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let mut timestamp_bytes = [0u8; 8];
        self.file
            .read_exact(&mut timestamp_bytes)
            .map_err(|error| io_error(error, 0, 0))?;
        let timestamp = i64::from_le_bytes(timestamp_bytes);
        let source_sequence = read_u64(&mut self.file, 0, 0)?;
        let line = read_u64(&mut self.file, 0, 0)?;
        let byte_offset = read_u64(&mut self.file, line, 0)?;
        let username_len = u64::from(read_u32(&mut self.file, line, byte_offset)?);
        let path_len = u64::from(read_u32(&mut self.file, line, byte_offset)?);
        if username_len > u32::MAX as u64 || path_len > u32::MAX as u64 {
            return Err(IngestError::new(
                ErrorCode::Io,
                line,
                byte_offset,
                "",
                "temporary sort run contains an oversized field",
            ));
        }
        let username_len = username_len as usize;
        let path_len = path_len as usize;
        let mut action = [0u8; 1];
        let mut directory = [0u8; 1];
        let mut colour_tag = [0u8; 1];
        self.file
            .read_exact(&mut action)
            .and_then(|_| self.file.read_exact(&mut directory))
            .and_then(|_| self.file.read_exact(&mut colour_tag))
            .map_err(|error| io_error(error, line, byte_offset))?;
        let colour = match colour_tag[0] {
            0 => None,
            1 => {
                let mut bytes = [0u8; 3];
                self.file
                    .read_exact(&mut bytes)
                    .map_err(|error| io_error(error, line, byte_offset))?;
                Some(ParsedColour(bytes))
            }
            _ => {
                return Err(IngestError::new(
                    ErrorCode::Io,
                    line,
                    byte_offset,
                    "",
                    "temporary sort run contains an invalid colour marker",
                ));
            }
        };
        if directory[0] > 1
            || username_len as u64 > options.limits.max_contributor_bytes
            || path_len as u64 > options.limits.max_path_bytes
        {
            return Err(IngestError::new(
                ErrorCode::Io,
                line,
                byte_offset,
                "",
                "temporary sort run violates configured field bounds",
            ));
        }
        let string_bytes = (username_len as u64).saturating_add(path_len as u64);
        memory.reserve(string_bytes, line, byte_offset)?;
        let mut username_bytes = Vec::new();
        username_bytes
            .try_reserve_exact(username_len)
            .map_err(|_| {
                working_memory_error(
                    memory.used.saturating_add(username_len as u64),
                    memory.limit,
                    line,
                    byte_offset,
                )
            })?;
        username_bytes.resize(username_len, 0);
        self.file
            .read_exact(&mut username_bytes)
            .map_err(|error| io_error(error, line, byte_offset))?;
        let username = String::from_utf8(username_bytes).map_err(|_| {
            IngestError::new(
                ErrorCode::Io,
                line,
                byte_offset,
                "",
                "temporary sort run contains invalid UTF-8",
            )
        })?;
        let mut path_bytes = Vec::new();
        path_bytes.try_reserve_exact(path_len).map_err(|_| {
            working_memory_error(
                memory.used.saturating_add(path_len as u64),
                memory.limit,
                line,
                byte_offset,
            )
        })?;
        path_bytes.resize(path_len, 0);
        self.file
            .read_exact(&mut path_bytes)
            .map_err(|error| io_error(error, line, byte_offset))?;
        let path = String::from_utf8(path_bytes).map_err(|_| {
            IngestError::new(
                ErrorCode::Io,
                line,
                byte_offset,
                "",
                "temporary sort run contains invalid UTF-8",
            )
        })?;
        self.remaining -= 1;
        let action = match action[0] {
            b'A' => ParsedAction::Add,
            b'M' => ParsedAction::Modify,
            b'D' => ParsedAction::Delete,
            _ => {
                return Err(IngestError::new(
                    ErrorCode::Io,
                    line,
                    byte_offset,
                    "",
                    "temporary sort run contains an invalid action",
                ));
            }
        };
        Ok(Some(NormalizedRecord {
            timestamp,
            username,
            action,
            path,
            is_directory: directory[0] != 0,
            colour,
            source_sequence,
            line,
            byte_offset,
        }))
    }
}

struct HeapEntry {
    timestamp: i64,
    source_sequence: u64,
    run_index: usize,
    record: NormalizedRecord,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.timestamp == other.timestamp
            && self.source_sequence == other.source_sequence
            && self.run_index == other.run_index
    }
}

impl Eq for HeapEntry {}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        other
            .timestamp
            .cmp(&self.timestamp)
            .then_with(|| other.source_sequence.cmp(&self.source_sequence))
            .then_with(|| other.run_index.cmp(&self.run_index))
    }
}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
struct MergeMemory {
    limit: u64,
    used: u64,
}

impl MergeMemory {
    fn new(
        run_count: usize,
        resident_metadata_bytes: u64,
        options: &IngestOptions,
    ) -> Result<Self, IngestError> {
        let requested = resident_metadata_bytes
            .saturating_add(vec_bytes::<RunReader>(run_count))
            .saturating_add(vec_bytes::<HeapEntry>(run_count));
        parser_memory_check(requested, options, 0, 0)?;
        Ok(Self {
            limit: parser_memory_limit(options),
            used: requested,
        })
    }

    fn reserve(&mut self, bytes: u64, line: u64, byte_offset: u64) -> Result<(), IngestError> {
        let requested = self
            .used
            .checked_add(bytes)
            .ok_or_else(|| working_memory_error(u64::MAX, self.limit, line, byte_offset))?;
        if requested > self.limit {
            return Err(working_memory_error(
                requested,
                self.limit,
                line,
                byte_offset,
            ));
        }
        self.used = requested;
        Ok(())
    }

    fn release(&mut self, bytes: u64) {
        self.used = self.used.saturating_sub(bytes);
    }
}

fn merge_memory_requirement(
    run_count: usize,
    resident_metadata_bytes: u64,
    options: &IngestOptions,
) -> Result<MergeMemory, IngestError> {
    MergeMemory::new(run_count, resident_metadata_bytes, options)
}

fn open_merge_readers(
    runs: &[SortRun],
    resident_metadata_bytes: u64,
    options: &IngestOptions,
) -> Result<(Vec<RunReader>, BinaryHeap<HeapEntry>, MergeMemory), IngestError> {
    let mut memory = merge_memory_requirement(runs.len(), resident_metadata_bytes, options)?;
    let mut readers = Vec::new();
    readers.try_reserve_exact(runs.len()).map_err(|_| {
        working_memory_error(
            memory
                .used
                .saturating_add(vec_bytes::<RunReader>(runs.len())),
            memory.limit,
            0,
            0,
        )
    })?;
    for run in runs {
        readers.push(RunReader::open(run)?);
    }
    let mut heap = BinaryHeap::new();
    heap.try_reserve_exact(runs.len()).map_err(|_| {
        working_memory_error(
            memory
                .used
                .saturating_add(vec_bytes::<HeapEntry>(runs.len())),
            memory.limit,
            0,
            0,
        )
    })?;
    for (run_index, reader) in readers.iter_mut().enumerate() {
        if let Some(record) = reader.next_record(options, &mut memory)? {
            heap.push(HeapEntry {
                timestamp: record.timestamp,
                source_sequence: record.source_sequence,
                run_index,
                record,
            });
        }
    }
    Ok((readers, heap, memory))
}

fn merge_group(
    runs: &[SortRun],
    state: &mut ParsedRuns,
    resident_metadata_bytes: u64,
    options: &IngestOptions,
) -> Result<SortRun, IngestError> {
    let run_id = state.next_run_id;
    state.next_run_id = state
        .next_run_id
        .checked_add(1)
        .ok_or_else(|| working_disk_error(u64::MAX, options.limits.working_disk_bytes, 0, 0))?;
    let path = state
        .tempdir
        .path()
        .join(format!("run-v1-{run_id:016x}.bin"));
    parser_memory_check(
        resident_metadata_bytes.saturating_add(path.as_os_str().len() as u64),
        options,
        0,
        0,
    )?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| io_error(error, 0, 0))?;
    let run_start = state.disk_bytes;
    let mut total = run_start
        .checked_add(RUN_HEADER_BYTES)
        .ok_or_else(|| working_disk_error(u64::MAX, options.limits.working_disk_bytes, 0, 0))?;
    if total > options.limits.working_disk_bytes {
        return Err(working_disk_error(
            total,
            options.limits.working_disk_bytes,
            0,
            0,
        ));
    }
    file.write_all(&RUN_MAGIC)
        .map_err(|error| io_error(error, 0, 0))?;
    let (mut readers, mut heap, mut memory) = open_merge_readers(
        runs,
        resident_metadata_bytes.saturating_add(path.as_os_str().len() as u64),
        options,
    )?;
    let mut record_count = 0u64;
    while let Some(entry) = heap.pop() {
        let run_index = entry.run_index;
        let entry_record_bytes = record_string_bytes(&entry.record);
        let line = entry.record.line;
        let byte_offset = entry.record.byte_offset;
        options.check_cancelled(line, byte_offset)?;
        let encoded = record_disk_bytes(&entry.record)?;
        let next_total = total.checked_add(encoded).ok_or_else(|| {
            working_disk_error(
                u64::MAX,
                options.limits.working_disk_bytes,
                line,
                byte_offset,
            )
        })?;
        if next_total > options.limits.working_disk_bytes {
            return Err(working_disk_error(
                next_total,
                options.limits.working_disk_bytes,
                line,
                byte_offset,
            ));
        }
        write_normalized_record(&mut file, &entry.record)?;
        total = next_total;
        record_count = record_count.saturating_add(1);
        drop(entry);
        memory.release(entry_record_bytes);
        if let Some(record) = readers[run_index].next_record(options, &mut memory)? {
            heap.push(HeapEntry {
                timestamp: record.timestamp,
                source_sequence: record.source_sequence,
                run_index,
                record,
            });
        }
    }
    file.flush().map_err(|error| io_error(error, 0, 0))?;
    let bytes = total
        .checked_sub(run_start)
        .ok_or_else(|| working_disk_error(u64::MAX, options.limits.working_disk_bytes, 0, 0))?;
    state.disk_bytes = total;
    Ok(SortRun {
        path,
        records: record_count,
        bytes,
    })
}

fn merge_all_runs(state: &mut ParsedRuns, options: &IngestOptions) -> Result<(), IngestError> {
    let fan_in = usize::try_from(options.limits.run_fan_in).unwrap_or(usize::MAX);
    if state.runs.len() <= fan_in {
        return Ok(());
    }
    if fan_in < 2 {
        return Err(limit_error(
            ErrorCode::RunFanInLimit,
            "run_fan_in",
            options.limits.run_fan_in,
        ));
    }
    let mut input_runs = std::mem::take(&mut state.runs);
    while input_runs.len() > fan_in {
        let next_capacity = input_runs.len().div_ceil(fan_in);
        let mut next_runs = Vec::new();
        let metadata_before_reserve = parsed_runs_memory_bytes(state)
            .saturating_add(sort_runs_memory_bytes(&input_runs, input_runs.capacity()))
            .saturating_add(vec_bytes::<SortRun>(next_capacity));
        parser_memory_check(metadata_before_reserve, options, 0, 0)?;
        next_runs.try_reserve_exact(next_capacity).map_err(|_| {
            working_memory_error(metadata_before_reserve, parser_memory_limit(options), 0, 0)
        })?;
        parser_memory_check(
            parsed_runs_memory_bytes(state)
                .saturating_add(sort_runs_memory_bytes(&input_runs, input_runs.capacity()))
                .saturating_add(sort_runs_memory_bytes(&next_runs, next_runs.capacity())),
            options,
            0,
            0,
        )?;
        for group in input_runs.chunks(fan_in) {
            options.check_cancelled(0, 0)?;
            let resident_metadata_bytes = parsed_runs_memory_bytes(state)
                .saturating_add(sort_runs_memory_bytes(&input_runs, input_runs.capacity()))
                .saturating_add(sort_runs_memory_bytes(&next_runs, next_runs.capacity()));
            let merged = merge_group(group, state, resident_metadata_bytes, options)?;
            let old_bytes = group
                .iter()
                .try_fold(0u64, |total, run| total.checked_add(run.bytes))
                .ok_or_else(|| {
                    working_disk_error(u64::MAX, options.limits.working_disk_bytes, 0, 0)
                })?;
            for run in group {
                fs::remove_file(&run.path).map_err(|error| io_error(error, 0, 0))?;
            }
            state.disk_bytes = state.disk_bytes.checked_sub(old_bytes).ok_or_else(|| {
                working_disk_error(u64::MAX, options.limits.working_disk_bytes, 0, 0)
            })?;
            next_runs.push(merged);
        }
        input_runs = next_runs;
    }
    state.runs = input_runs;
    parser_memory_check(parsed_runs_memory_bytes(state), options, 0, 0)?;
    Ok(())
}

pub(crate) fn consume_sorted_records<F>(
    parsed: &mut ParsedInput,
    options: &IngestOptions,
    mut on_record: F,
) -> Result<(), IngestError>
where
    F: FnMut(NormalizedRecord) -> Result<(), IngestError>,
{
    if let Some(state) = parsed.runs.as_mut() {
        merge_all_runs(state, options)?;
        let runs = &state.runs;
        let resident_metadata_bytes = parsed_runs_memory_bytes(state);
        let (mut readers, mut heap, mut memory) =
            open_merge_readers(runs, resident_metadata_bytes, options)?;
        while let Some(entry) = heap.pop() {
            let entry_record_bytes = record_string_bytes(&entry.record);
            let run_index = entry.run_index;
            let line = entry.record.line;
            let byte_offset = entry.record.byte_offset;
            options.check_cancelled(line, byte_offset)?;
            on_record(entry.record)?;
            memory.release(entry_record_bytes);
            if let Some(record) = readers[run_index].next_record(options, &mut memory)? {
                heap.push(HeapEntry {
                    timestamp: record.timestamp,
                    source_sequence: record.source_sequence,
                    run_index,
                    record,
                });
            }
        }
    } else {
        parsed
            .records
            .sort_unstable_by_key(|record| (record.timestamp, record.source_sequence));
        for record in parsed.records.drain(..) {
            options.check_cancelled(record.line, record.byte_offset)?;
            on_record(record)?;
        }
    }
    Ok(())
}

fn read_physical_record<R: BufRead>(
    input: &mut R,
    options: &IngestOptions,
    line: u64,
    byte_offset: u64,
    input_hash: &mut blake3::Hasher,
    input_bytes: &mut u64,
) -> Result<Option<PhysicalRecord>, IngestError> {
    let max_record = options.limits.max_record_bytes;
    let max_memory = parser_payload_bytes(parser_memory_limit(options));
    let mut bytes = Vec::new();
    loop {
        let current_offset = byte_offset.checked_add(bytes.len() as u64).ok_or_else(|| {
            IngestError::new(
                ErrorCode::InputTooLarge,
                line,
                byte_offset,
                "",
                "byte offset overflows u64",
            )
        })?;
        options.check_cancelled(line, current_offset)?;
        let available = input
            .fill_buf()
            .map_err(|error| io_error(error, line, current_offset))?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            return Ok(Some(PhysicalRecord {
                bytes,
                terminated: false,
                line,
                byte_offset,
            }));
        }

        let newline = available.iter().position(|&byte| byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        let new_record_len = bytes.len().checked_add(take).ok_or_else(|| {
            IngestError::new(
                ErrorCode::RecordTooLarge,
                line,
                byte_offset,
                bounded_context(&bytes),
                "physical record length overflows usize",
            )
        })?;
        if new_record_len as u64 > max_record {
            return Err(IngestError::new(
                ErrorCode::RecordTooLarge,
                line,
                byte_offset,
                bounded_context(&bytes),
                "physical record including delimiter exceeds configured byte limit",
            ));
        }
        if new_record_len as u64 > max_memory {
            return Err(working_memory_error(
                new_record_len as u64,
                max_memory,
                line,
                byte_offset,
            ));
        }
        let new_input_len = input_bytes.checked_add(take as u64).ok_or_else(|| {
            IngestError::new(
                ErrorCode::InputTooLarge,
                line,
                byte_offset,
                bounded_context(&bytes),
                "finite source byte count overflows u64",
            )
        })?;
        if new_input_len > options.limits.max_input_bytes {
            return Err(IngestError::new(
                ErrorCode::InputTooLarge,
                line,
                byte_offset,
                bounded_context(&bytes),
                "finite source exceeds configured byte limit",
            ));
        }
        bytes.try_reserve_exact(take).map_err(|_| {
            working_memory_error(new_record_len as u64, max_memory, line, byte_offset)
        })?;
        bytes.extend_from_slice(&available[..take]);
        input_hash.update(&available[..take]);
        input.consume(take);
        *input_bytes = new_input_len;
        if newline.is_some() {
            let body_len = bytes.len().saturating_sub(1);
            if body_len > 0 && bytes[body_len - 1] == b'\r' {
                bytes.truncate(body_len - 1);
            } else {
                bytes.truncate(body_len);
            }
            return Ok(Some(PhysicalRecord {
                bytes,
                terminated: true,
                line,
                byte_offset,
            }));
        }
    }
}

fn parse_record(
    body: &[u8],
    line: u64,
    byte_offset: u64,
    limits: &IngestLimits,
) -> Result<NormalizedRecord, IngestError> {
    let text = std::str::from_utf8(body).map_err(|_| {
        IngestError::new(
            ErrorCode::InvalidUtf8,
            line,
            byte_offset,
            bounded_context(body),
            "record is not valid UTF-8",
        )
    })?;
    let mut fields = text.split('|');
    let first = fields.next();
    let second = fields.next();
    let third = fields.next();
    let fourth = fields.next();
    let fifth = fields.next();
    let mut field_count = [first, second, third, fourth, fifth]
        .into_iter()
        .filter(Option::is_some)
        .count();
    field_count = field_count.saturating_add(fields.count());
    if field_count != 4 && field_count != 5 {
        return Err(IngestError::new(
            ErrorCode::WrongFieldCount,
            line,
            byte_offset,
            bounded_context(body),
            format!("expected exactly four or five fields, got {field_count}"),
        ));
    }
    let timestamp_text = first.expect("field count checked");
    let username_text = second.expect("field count checked");
    let action_text = third.expect("field count checked");
    let path_text = fourth.expect("field count checked");
    if field_count == 5 && fifth.expect("field count checked").is_empty() {
        return Err(IngestError::new(
            ErrorCode::WrongFieldCount,
            line,
            byte_offset,
            bounded_context(body),
            "the optional colour field cannot be empty",
        ));
    }
    let timestamp = parse_timestamp(timestamp_text).map_err(|message| {
        IngestError::new(
            ErrorCode::InvalidTimestamp,
            line,
            byte_offset,
            bounded_context(body),
            message,
        )
    })?;
    let username_text = if username_text.is_empty() {
        "Unknown"
    } else {
        username_text
    };
    if username_text.len() as u64 > limits.max_contributor_bytes {
        return Err(IngestError::new(
            ErrorCode::ContributorTooLong,
            line,
            byte_offset,
            bounded_context(body),
            "contributor exceeds configured byte limit",
        ));
    }
    let mut username = String::new();
    username
        .try_reserve_exact(username_text.len())
        .map_err(|_| {
            working_memory_error(
                username_text.len() as u64,
                parser_working_memory_bytes(limits.working_memory_bytes),
                line,
                byte_offset,
            )
        })?;
    username.push_str(username_text);
    let action = match action_text {
        "" | "A" => ParsedAction::Add,
        "M" => ParsedAction::Modify,
        "D" => ParsedAction::Delete,
        _ => {
            return Err(IngestError::new(
                ErrorCode::InvalidAction,
                line,
                byte_offset,
                bounded_context(body),
                "action must be empty, A, M, or D",
            ));
        }
    };
    let colour = fifth
        .filter(|_| field_count == 5)
        .map(parse_colour)
        .transpose()
        .map_err(|message| {
            IngestError::new(
                ErrorCode::InvalidColour,
                line,
                byte_offset,
                bounded_context(body),
                message,
            )
        })?;
    let (path, is_directory) = parse_path(path_text, limits).map_err(|(code, message)| {
        IngestError::new(code, line, byte_offset, bounded_context(body), message)
    })?;
    if is_directory && action != ParsedAction::Delete {
        return Err(IngestError::new(
            ErrorCode::DirectoryActionUnsupported,
            line,
            byte_offset,
            bounded_context(body),
            "directory targets support only delete actions",
        ));
    }
    Ok(NormalizedRecord {
        timestamp,
        username,
        action,
        path,
        is_directory,
        colour,
        source_sequence: 0,
        line,
        byte_offset,
    })
}

fn parse_colour(value: &str) -> Result<ParsedColour, String> {
    let digits = value.strip_prefix('#').unwrap_or(value);
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(
            "colour must contain exactly six hexadecimal digits, optionally prefixed by #"
                .to_owned(),
        );
    }
    let bytes = digits.as_bytes();
    let mut rgb = [0u8; 3];
    for (index, slot) in rgb.iter_mut().enumerate() {
        let high = hex_digit(bytes[index * 2]).expect("validated hex digit");
        let low = hex_digit(bytes[index * 2 + 1]).expect("validated hex digit");
        *slot = high * 16 + low;
    }
    Ok(ParsedColour(rgb))
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_path(value: &str, limits: &IngestLimits) -> Result<(String, bool), (ErrorCode, String)> {
    if value.is_empty() {
        return Err((ErrorCode::InvalidPath, "path cannot be empty".to_owned()));
    }
    if value.len() as u64 > limits.max_path_bytes {
        return Err((
            ErrorCode::PathTooLong,
            "path exceeds configured byte limit".to_owned(),
        ));
    }
    let mut path = value;
    if let Some(stripped) = path.strip_prefix('/') {
        path = stripped;
    }
    let is_directory = path.ends_with('/');
    if is_directory {
        path = &path[..path.len() - 1];
    }
    if path.is_empty() {
        return Err((
            ErrorCode::InvalidPath,
            "root or empty path is not a file target".to_owned(),
        ));
    }
    let mut component_count = 0u64;
    for component in path.split('/') {
        component_count = component_count.saturating_add(1);
        if component.is_empty() || component == "." || component == ".." {
            return Err((
                ErrorCode::InvalidPath,
                "path contains an empty, . or .. component".to_owned(),
            ));
        }
    }
    if component_count > limits.max_path_components {
        return Err((
            ErrorCode::PathTooDeep,
            "path exceeds configured component limit".to_owned(),
        ));
    }
    let mut normalized = String::new();
    normalized.try_reserve_exact(path.len()).map_err(|_| {
        (
            ErrorCode::WorkingMemoryLimit,
            "working memory cannot hold normalized path".to_owned(),
        )
    })?;
    normalized.push_str(path);
    Ok((normalized, is_directory))
}

fn parse_timestamp(value: &str) -> Result<i64, String> {
    if value.is_empty() {
        return Err("timestamp cannot be empty".to_owned());
    }
    let bytes = value.as_bytes();
    let epoch_digits = if bytes[0] == b'-' { &bytes[1..] } else { bytes };
    if !epoch_digits.is_empty() && epoch_digits.iter().all(u8::is_ascii_digit) {
        if bytes[0] == b'+' {
            return Err("leading + is not accepted for epoch timestamps".to_owned());
        }
        return value
            .parse::<i64>()
            .map_err(|_| "signed epoch timestamp overflows i64".to_owned());
    }
    if bytes[0] == b'+' {
        return Err("leading + is not accepted for epoch timestamps".to_owned());
    }
    parse_calendar_timestamp(value)
}

fn parse_calendar_timestamp(value: &str) -> Result<i64, String> {
    let bytes = value.as_bytes();
    if bytes.len() < 10 {
        return Err("timestamp is not a supported date form".to_owned());
    }
    let year = parse_fixed_u32(&bytes[0..4]).ok_or_else(|| "invalid four-digit year".to_owned())?;
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return Err("date must use YYYY-MM-DD".to_owned());
    }
    let month = parse_fixed_u32(&bytes[5..7]).ok_or_else(|| "invalid month".to_owned())?;
    let day = parse_fixed_u32(&bytes[8..10]).ok_or_else(|| "invalid day".to_owned())?;
    let mut rest = &bytes[10..];
    let mut hour = 0u32;
    let mut minute = 0u32;
    let mut second = 0u32;
    let rfc3339 = if rest.first() == Some(&b'T') {
        if rest.len() < 9 || rest[3] != b':' || rest[6] != b':' {
            return Err("RFC3339 timestamp must use HH:MM:SS".to_owned());
        }
        hour = parse_fixed_u32(&rest[1..3]).ok_or_else(|| "invalid hour".to_owned())?;
        minute = parse_fixed_u32(&rest[4..6]).ok_or_else(|| "invalid minute".to_owned())?;
        second = parse_fixed_u32(&rest[7..9]).ok_or_else(|| "invalid second".to_owned())?;
        rest = &rest[9..];
        true
    } else if rest.first() == Some(&b' ') {
        if rest.len() < 6 {
            return Err("legacy date time must use HH:MM or HH:MM:SS".to_owned());
        }
        hour = parse_fixed_u32(&rest[1..3]).ok_or_else(|| "invalid hour".to_owned())?;
        if rest[3] != b':' {
            return Err("legacy date time must use HH:MM".to_owned());
        }
        minute = parse_fixed_u32(&rest[4..6]).ok_or_else(|| "invalid minute".to_owned())?;
        rest = &rest[6..];
        if rest.first() == Some(&b':') {
            if rest.len() < 3 {
                return Err("legacy date time must use HH:MM:SS".to_owned());
            }
            second = parse_fixed_u32(&rest[1..3]).ok_or_else(|| "invalid second".to_owned())?;
            rest = &rest[3..];
        }
        false
    } else {
        false
    };

    let offset_seconds = if rfc3339 {
        parse_rfc_offset(rest)?
    } else {
        let had_separator = rest.first() == Some(&b' ');
        let rest = if had_separator { &rest[1..] } else { rest };
        if had_separator && rest.is_empty() {
            return Err("timestamp has a separator without an explicit offset".to_owned());
        }
        if rest.is_empty() {
            0i64
        } else {
            parse_legacy_offset(rest)?
        }
    };
    if month == 0 || month > 12 || day == 0 || day > days_in_month(year as i64, month) {
        return Err("date is outside the Gregorian calendar".to_owned());
    }
    if hour > 23 || minute > 59 || second > 59 {
        return Err("time is outside the valid range".to_owned());
    }
    let days = days_from_civil(year as i64, month, day);
    let seconds = days
        .checked_mul(86_400)
        .and_then(|value| value.checked_add(hour as i64 * 3_600))
        .and_then(|value| value.checked_add(minute as i64 * 60))
        .and_then(|value| value.checked_add(second as i64))
        .and_then(|value| value.checked_sub(offset_seconds))
        .ok_or_else(|| "date timestamp overflows i64".to_owned())?;
    Ok(seconds)
}

fn parse_rfc_offset(value: &[u8]) -> Result<i64, String> {
    if value == b"Z" {
        return Ok(0);
    }
    parse_offset(value, true)
}

fn parse_legacy_offset(value: &[u8]) -> Result<i64, String> {
    parse_offset(value, false)
}

fn parse_offset(value: &[u8], require_minutes: bool) -> Result<i64, String> {
    if value.len() < 3 || (value[0] != b'+' && value[0] != b'-') {
        return Err("timestamp offset must use +HH[:MM] or -HH[:MM]".to_owned());
    }
    let hour = parse_fixed_u32(&value[1..3]).ok_or_else(|| "invalid offset hour".to_owned())?;
    let minute = if value.len() == 3 && !require_minutes {
        0
    } else {
        if value.len() != 6 || value[3] != b':' {
            return Err("timestamp offset must use +HH:MM or -HH:MM".to_owned());
        }
        parse_fixed_u32(&value[4..6]).ok_or_else(|| "invalid offset minute".to_owned())?
    };
    if hour > 23 || minute > 59 {
        return Err("timestamp offset is outside the valid range".to_owned());
    }
    let total = hour as i64 * 3_600 + minute as i64 * 60;
    Ok(if value[0] == b'-' { -total } else { total })
}

fn parse_fixed_u32(value: &[u8]) -> Option<u32> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut number = 0u32;
    for &byte in value {
        number = number.checked_mul(10)?.checked_add((byte - b'0') as u32)?;
    }
    Some(number)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.rem_euclid(4) == 0
            && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0) =>
        {
            29
        }
        2 => 28,
        _ => 0,
    }
}

// Howard Hinnant's proleptic Gregorian conversion, with an epoch of
// 1970-01-01.  The arithmetic is bounded to four-digit input years here, but
// checked operations are retained at the final timestamp conversion.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = (if year >= 0 { year } else { year - 399 }) / 400;
    let year_of_era = year - era * 400;
    let month = month as i64;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn bounded_context(bytes: &[u8]) -> String {
    const MAX_CONTEXT: usize = 96;
    let mut output = String::new();
    for &byte in bytes.iter().take(MAX_CONTEXT) {
        match byte {
            b'\\' => output.push_str("\\\\"),
            b'\n' => output.push_str("\\n"),
            b'\r' => output.push_str("\\r"),
            b'\t' => output.push_str("\\t"),
            0x20..=0x7e => output.push(byte as char),
            _ => output.push_str(&format!("\\x{byte:02x}")),
        }
    }
    if bytes.len() > MAX_CONTEXT {
        output.push('…');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse_reader(input: &[u8], options: &IngestOptions) -> Result<ParsedInput, IngestError> {
        super::parse_reader_with_callback(input, options, |_record, _working_bytes| Ok(()))
    }

    fn parse(input: &[u8]) -> ParsedInput {
        parse_reader(input, &IngestOptions::default()).expect("valid input")
    }

    #[test]
    fn parses_defaults_bom_and_crlf() {
        let parsed = parse("\u{feff}0||A|/src/main.rs|#80c0ff\r\n1|||docs/readme.md\n".as_bytes());
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.records[0].username, "Unknown");
        assert_eq!(parsed.records[0].path, "src/main.rs");
        assert_eq!(
            parsed.records[0].colour,
            Some(ParsedColour([0x80, 0xc0, 0xff]))
        );
        assert_eq!(parsed.records[1].action, ParsedAction::Add);
    }

    #[test]
    fn sorts_are_left_to_caller_and_sequence_is_physical() {
        let parsed = parse(b"2|a|A|b\n1|a|A|a\n2|a|A|c\n");
        assert_eq!(parsed.records[0].source_sequence, 0);
        assert_eq!(parsed.records[1].source_sequence, 1);
        assert_eq!(parsed.records[2].source_sequence, 2);
    }

    #[test]
    fn strict_record_and_path_errors_have_location() {
        let error = parse_reader(b"1|a|A|a|\n".as_slice(), &IngestOptions::default()).unwrap_err();
        assert_eq!(error.code(), "wrong-field-count");
        assert_eq!(error.line, 1);
        assert_eq!(error.byte_offset, 0);
    }

    #[test]
    fn dates_are_utc_and_offsets_are_applied() {
        let utc = parse("1970-01-01T00:00:00Z|a|A|a\n".as_bytes());
        let shifted = parse("1970-01-01 02:00:00 +02:00|a|A|b\n".as_bytes());
        assert_eq!(utc.records[0].timestamp, 0);
        assert_eq!(shifted.records[0].timestamp, 0);
    }

    #[test]
    fn rejects_late_bom_and_lone_cr() {
        let late_bom = parse_reader(
            b"0|a|A|a\n\xef\xbb\xbf1|a|A|b\n".as_slice(),
            &IngestOptions::default(),
        )
        .unwrap_err();
        assert_eq!(late_bom.code(), "bom-not-at-start");
        let lone_cr =
            parse_reader(b"0|a|A|a\r1|a|A|b\n".as_slice(), &IngestOptions::default()).unwrap_err();
        assert_eq!(lone_cr.code(), "lone-carriage-return");
    }

    #[test]
    fn rejects_limits_without_publishing_prefix() {
        let options = IngestOptions {
            limits: IngestLimits::default().with_max_events(1),
            ..IngestOptions::default()
        };
        let error = parse_reader(b"0|a|A|a\n1|a|A|b\n".as_slice(), &options).unwrap_err();
        assert_eq!(error.code(), "event-count-limit");
        let options = IngestOptions {
            limits: IngestLimits::default().with_max_record_bytes(7),
            ..IngestOptions::default()
        };
        let error = parse_reader(b"0|a|A|a\n".as_slice(), &options).unwrap_err();
        assert_eq!(error.code(), "record-too-large");
    }

    #[test]
    fn cancellation_is_not_eof() {
        let token = CancellationToken::new();
        token.cancel();
        let options = IngestOptions::default().with_cancellation(token);
        let error = parse_reader(b"".as_slice(), &options).unwrap_err();
        assert_eq!(error.code(), "cancelled");
        assert!(error.line > 0);
    }

    #[test]
    fn lexical_path_rules_keep_backslashes_and_reject_traversal() {
        let parsed = parse(br#"0|a|A|dir\name/file"#);
        assert_eq!(parsed.records[0].path, r#"dir\name/file"#);
        let error =
            parse_reader(b"0|a|A|a/../b\n".as_slice(), &IngestOptions::default()).unwrap_err();
        assert_eq!(error.code(), "invalid-path");
    }

    #[test]
    fn date_forms_reject_fraction_and_trailing_space() {
        let fraction = parse_reader(
            b"1970-01-01T00:00:00.1Z|a|A|a\n".as_slice(),
            &IngestOptions::default(),
        )
        .unwrap_err();
        assert_eq!(fraction.code(), "invalid-timestamp");
        let trailing =
            parse_reader(b"1970-01-01 |a|A|a\n".as_slice(), &IngestOptions::default()).unwrap_err();
        assert_eq!(trailing.code(), "invalid-timestamp");
    }
    #[test]
    fn temporary_sort_runs_are_removed_when_state_drops() {
        let options = IngestOptions::default().with_limits(
            IngestLimits::default()
                .with_working_memory_bytes(8 * 1024)
                .with_working_disk_bytes(1 << 20),
        );
        let mut parsed = parse(b"2|a|A|b\n1|a|A|a\n");
        let mut records = std::mem::take(&mut parsed.records);
        let mut chunk_bytes = 0;
        let mut runs = None;
        spill_records(&mut records, &mut chunk_bytes, &mut runs, &options)
            .expect("write temporary sort run");

        let state = runs.as_ref().expect("spill creates run state");
        let tempdir = state.tempdir.path().to_owned();
        let run_path = state
            .runs
            .first()
            .expect("spill creates one run")
            .path
            .clone();
        assert!(tempdir.is_dir());
        assert!(run_path.is_file());

        drop(runs);
        assert!(!run_path.exists());
        assert!(!tempdir.exists());
    }
}
