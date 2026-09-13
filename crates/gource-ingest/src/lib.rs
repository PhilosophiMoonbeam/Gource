// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Finite custom-log ingestion over the presentation-independent core.
//!
//! The reader is deliberately byte bounded: it never uses `read_to_end`,
//! never executes a shell, and never publishes a partially parsed history.

mod cache;
mod git;
mod parser;

pub use cache::{CacheConfig, CacheError, CacheKey, DEFAULT_CACHE_BYTES, EventCache};

pub use git::{GitError, GitOptions, ingest_git_repository};

use std::borrow::Cow;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::mem::size_of;
use std::path::{Path, PathBuf};

use blake3::Hasher;
use gource_core::{
    Action, Catalog, CatalogLimits, ContributorId, Event, EventKey, EventTarget, Generation,
    HistorySource, PathId, RepositoryPath, Rgb8, SourceSeq,
};

pub use parser::{
    CancellationToken, DEFAULT_CONTRIBUTOR_BYTES, DEFAULT_INPUT_BYTES, DEFAULT_PATH_BYTES,
    DEFAULT_PATH_COMPONENTS, DEFAULT_RECORD_BYTES, DEFAULT_RUN_FAN_IN, DEFAULT_WORKING_DISK_BYTES,
    DEFAULT_WORKING_MEMORY_BYTES, ErrorCode, IngestError, IngestLimits, IngestOptions,
    ParsedAction, ProgressPhase, ProgressSink, ProgressUpdate,
};

/// A finite source selected by the caller.  `Stdin` is intentionally finite:
/// the parser waits for EOF before exposing an index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputSpec {
    File(PathBuf),
    Stdin,
}

impl InputSpec {
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self::File(path.into())
    }

    pub fn stdin() -> Self {
        Self::Stdin
    }
}

impl From<PathBuf> for InputSpec {
    fn from(value: PathBuf) -> Self {
        Self::File(value)
    }
}

impl From<&Path> for InputSpec {
    fn from(value: &Path) -> Self {
        Self::File(value.to_owned())
    }
}

impl From<&str> for InputSpec {
    fn from(value: &str) -> Self {
        if value == "-" {
            Self::Stdin
        } else {
            Self::File(PathBuf::from(value))
        }
    }
}

impl From<String> for InputSpec {
    fn from(value: String) -> Self {
        Self::from(value.as_str())
    }
}

/// Stable digest of the exact finite source bytes consumed by the parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InputIdentity {
    digest: [u8; 32],
    bytes: u64,
}

impl InputIdentity {
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.digest
    }
}

impl fmt::Display for InputIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.digest {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Stable digest of normalized catalog and canonical event content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DatasetIdentity([u8; 32]);

impl DatasetIdentity {
    pub fn digest(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}
/// Short identity aliases used by callers that use `Id` rather than
/// `Identity` in their source adapter vocabulary.
pub type InputId = InputIdentity;
pub type DatasetId = DatasetIdentity;

impl fmt::Display for DatasetIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// A complete, immutable finite history.  The catalog and event vector are
/// private so callers cannot mutate identities after publication.
#[derive(Clone, Eq, PartialEq)]
pub struct IndexedHistory {
    catalog: Catalog,
    events: Vec<Event>,
    input_identity: InputIdentity,
    dataset_identity: DatasetIdentity,
    input_bytes: u64,
}

impl fmt::Debug for IndexedHistory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexedHistory")
            .field("events", &self.events.len())
            .field("input_identity", &self.input_identity)
            .field("dataset_identity", &self.dataset_identity)
            .field("input_bytes", &self.input_bytes)
            .finish()
    }
}

impl IndexedHistory {
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn event(&self, index: gource_core::EventIndex) -> Option<&Event> {
        self.events.get(index.get() as usize)
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn input_identity(&self) -> InputIdentity {
        self.input_identity
    }

    pub fn dataset_identity(&self) -> DatasetIdentity {
        self.dataset_identity
    }

    pub fn input_id(&self) -> InputIdentity {
        self.input_identity
    }

    pub fn dataset_id(&self) -> DatasetIdentity {
        self.dataset_identity
    }

    pub fn input_bytes(&self) -> u64 {
        self.input_bytes
    }
}

impl HistorySource for IndexedHistory {
    fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    fn len(&self) -> usize {
        self.events.len()
    }

    fn event(&self, index: gource_core::EventIndex) -> Option<&Event> {
        self.events.get(index.get() as usize)
    }

    fn events(&self) -> &[Event] {
        &self.events
    }
}

pub fn parse_reader<R: Read>(
    reader: R,
    options: impl Into<IngestOptions>,
) -> Result<IndexedHistory, IngestError> {
    let options = options.into();
    options.limits.validate()?;
    let maximum = index_memory_limit(&options);
    check_index_memory(CATALOG_MEMORY_BASE, 0, 0, maximum, 0, 0)?;
    let mut catalog = new_catalog(&options);
    let mut catalog_bytes = CATALOG_MEMORY_BASE;
    let parsed = parser::parse_reader_with_callback(reader, &options, |record, working_bytes| {
        intern_record(
            &mut catalog,
            &mut catalog_bytes,
            record,
            working_bytes,
            &options,
        )
    })?;
    build_index(parsed, &options, catalog, catalog_bytes)
}

impl From<()> for IngestOptions {
    fn from(_: ()) -> Self {
        Self::default()
    }
}
impl From<&IngestOptions> for IngestOptions {
    fn from(value: &IngestOptions) -> Self {
        value.clone()
    }
}
impl From<IngestLimits> for IngestOptions {
    fn from(limits: IngestLimits) -> Self {
        Self {
            limits,
            ..Self::default()
        }
    }
}

/// Parse a byte slice as a finite custom log.
pub fn parse_bytes(
    bytes: &[u8],
    options: impl Into<IngestOptions>,
) -> Result<IndexedHistory, IngestError> {
    parse_reader(bytes, options)
}

/// Parse a regular custom-log file or a local Git repository directory.  The
/// path is data only; no shell or platform path canonicalization is performed.
pub fn parse_path(
    path: impl AsRef<Path>,
    options: impl Into<IngestOptions>,
) -> Result<IndexedHistory, IngestError> {
    let path = path.as_ref();
    let options = options.into();
    if path == Path::new("-") {
        return parse_stdin(options);
    }
    if path.is_dir() {
        let context = path.to_string_lossy();
        let context = if context.len() <= 256 {
            context.into_owned()
        } else {
            let mut end = 256;
            while !context.is_char_boundary(end) {
                end -= 1;
            }
            context[..end].to_owned()
        };
        return ingest_git_repository(path, &options, &GitOptions::default())
            .map_err(|error| IngestError::new(ErrorCode::Io, 0, 0, context, error.to_string()));
    }
    let file = File::open(path).map_err(|error| {
        IngestError::new(
            ErrorCode::Io,
            0,
            0,
            "",
            format!("{}: {}", path.display(), error),
        )
    })?;
    parse_reader(file, options)
}
/// Hash one regular finite file without buffering it in memory.
///
/// The source is read in fixed-size chunks, and one extra byte is probed when
/// the configured input limit is reached so an over-limit file is rejected
/// without ever producing an identity for a truncated prefix.
pub fn input_identity_path(
    path: impl AsRef<Path>,
    options: &IngestOptions,
) -> Result<InputIdentity, IngestError> {
    options.limits.validate()?;
    let path = path.as_ref();
    let context = input_path_context(path);
    let mut file = File::open(path).map_err(|error| {
        IngestError::new(
            ErrorCode::Io,
            0,
            0,
            context.clone(),
            format!("{}: {}", path.display(), error),
        )
    })?;
    let metadata = file.metadata().map_err(|error| {
        IngestError::new(
            ErrorCode::Io,
            0,
            0,
            context.clone(),
            format!("{}: {}", path.display(), error),
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(IngestError::new(
            ErrorCode::Io,
            0,
            0,
            context,
            "input path is not a regular file",
        ));
    }

    const HASH_BUFFER_BYTES: usize = 64 * 1024;
    let maximum = options.limits.max_input_bytes;
    let mut hasher = Hasher::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; HASH_BUFFER_BYTES];
    loop {
        options.check_cancelled(0, bytes)?;
        if bytes == maximum {
            let mut probe = [0_u8; 1];
            let read = loop {
                match file.read(&mut probe) {
                    Ok(read) => break read,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        return Err(IngestError::new(
                            ErrorCode::Io,
                            0,
                            bytes,
                            context.clone(),
                            format!("{}: {}", path.display(), error),
                        ));
                    }
                }
            };
            if read != 0 {
                return Err(IngestError::new(
                    ErrorCode::InputTooLarge,
                    0,
                    bytes,
                    context,
                    "finite source exceeds configured byte limit",
                ));
            }
            break;
        }

        let remaining = maximum - bytes;
        let chunk_size = usize::try_from(remaining)
            .unwrap_or(HASH_BUFFER_BYTES)
            .min(HASH_BUFFER_BYTES);
        let read = loop {
            match file.read(&mut buffer[..chunk_size]) {
                Ok(read) => break read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(IngestError::new(
                        ErrorCode::Io,
                        0,
                        bytes,
                        context.clone(),
                        format!("{}: {}", path.display(), error),
                    ));
                }
            }
        };
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes.checked_add(read as u64).ok_or_else(|| {
            IngestError::new(
                ErrorCode::InputTooLarge,
                0,
                bytes,
                context.clone(),
                "finite source byte count overflows u64",
            )
        })?;
    }

    Ok(InputIdentity {
        digest: *hasher.finalize().as_bytes(),
        bytes,
    })
}

fn input_path_context(path: &Path) -> String {
    let context = path.to_string_lossy();
    if context.len() <= 256 {
        context.into_owned()
    } else {
        let mut end = 256;
        while !context.is_char_boundary(end) {
            end -= 1;
        }
        context[..end].to_owned()
    }
}

/// Parse finite stdin to EOF.  This function does not spawn a process and
/// does not provide live/tailing semantics.
pub fn parse_stdin(options: impl Into<IngestOptions>) -> Result<IndexedHistory, IngestError> {
    parse_reader(io::stdin().lock(), options)
}

/// Parse the source selected by `InputSpec`.
pub fn parse_input(
    source: InputSpec,
    options: impl Into<IngestOptions>,
) -> Result<IndexedHistory, IngestError> {
    match source {
        InputSpec::File(path) => parse_path(path, options),
        InputSpec::Stdin => parse_stdin(options),
    }
}

const CATALOG_MEMORY_BASE: u64 = 4 * 1024;
const CATALOG_MAP_NODE_BYTES: u64 = 128;
const EVENTS_MEMORY_BASE: u64 = size_of::<Vec<Event>>() as u64;
const MEMORY_GROWTH_FACTOR: u64 = 2;

fn new_catalog(options: &IngestOptions) -> Catalog {
    Catalog::with_limits(CatalogLimits {
        path_bytes: usize_limit(options.limits.max_path_bytes),
        contributor_bytes: usize_limit(options.limits.max_contributor_bytes),
        path_components: usize_limit(options.limits.max_path_components),
        max_paths: usize_limit(options.limits.max_events),
        max_contributors: usize_limit(options.limits.max_events),
    })
}

fn index_memory_limit(options: &IngestOptions) -> u64 {
    parser::index_working_memory_bytes(options.limits.working_memory_bytes)
}

fn vector_capacity_bytes(length: u64, element_size: u64) -> Option<u64> {
    if length == 0 {
        return Some(0);
    }
    length
        .checked_mul(MEMORY_GROWTH_FACTOR)?
        .max(4)
        .checked_mul(element_size)
}

fn vector_growth_bytes(current_len: usize, element_size: u64) -> Option<u64> {
    let current_len = u64::try_from(current_len).ok()?;
    let old_capacity = vector_capacity_bytes(current_len, element_size)?;
    let new_capacity = vector_capacity_bytes(current_len.checked_add(1)?, element_size)?;
    new_capacity.checked_sub(old_capacity)
}

fn catalog_path_bytes(path: &RepositoryPath) -> Option<u64> {
    let component_count = u64::try_from(path.components().len()).ok()?;
    let component_bytes = path
        .components()
        .iter()
        .try_fold(0u64, |total, component| {
            total.checked_add(u64::try_from(component.capacity()).ok()?)
        })?;
    let canonical_capacity = u64::try_from(path.canonical().len())
        .ok()?
        .checked_mul(MEMORY_GROWTH_FACTOR)?;
    let path_metadata = u64::try_from(size_of::<RepositoryPath>()).ok()?;
    let component_metadata =
        vector_capacity_bytes(component_count, u64::try_from(size_of::<String>()).ok()?)?;
    let map_metadata = u64::try_from(size_of::<String>())
        .ok()?
        .checked_add(u64::try_from(size_of::<PathId>()).ok()?)?
        .checked_add(CATALOG_MAP_NODE_BYTES)?;
    path_metadata
        .checked_add(canonical_capacity)?
        .checked_add(component_bytes)?
        .checked_add(component_metadata)?
        .checked_add(canonical_capacity)?
        .checked_add(map_metadata)
}

fn catalog_contributor_bytes(contributor: &str) -> Option<u64> {
    let length = u64::try_from(contributor.len()).ok()?;
    length
        .checked_mul(2)?
        .checked_add(u64::try_from(size_of::<String>()).ok()?)?
        .checked_add(u64::try_from(size_of::<ContributorId>()).ok()?)?
        .checked_add(CATALOG_MAP_NODE_BYTES)
}

fn record_path_memory_bytes(record: &parser::NormalizedRecord) -> Option<u64> {
    let body_bytes = u64::try_from(record.path.len()).ok()?;
    let marker_bytes = if record.is_directory { 1u64 } else { 0 };
    let canonical_bytes = body_bytes.checked_add(marker_bytes)?;
    let component_count = u64::try_from(
        record
            .path
            .bytes()
            .filter(|&byte| byte == b'/')
            .count()
            .checked_add(1)?,
    )
    .ok()?;
    let path_metadata = u64::try_from(size_of::<RepositoryPath>()).ok()?;
    let component_metadata =
        vector_capacity_bytes(component_count, u64::try_from(size_of::<String>()).ok()?)?;
    let mut bytes = path_metadata
        .checked_add(canonical_bytes.checked_mul(MEMORY_GROWTH_FACTOR)?)?
        .checked_add(body_bytes.checked_mul(MEMORY_GROWTH_FACTOR)?)?
        .checked_add(component_metadata)?;
    if record.is_directory {
        bytes = bytes
            .checked_add(u64::try_from(size_of::<String>()).ok()?)?
            .checked_add(canonical_bytes.checked_mul(MEMORY_GROWTH_FACTOR)?)?;
    }
    Some(bytes)
}

fn record_path_text<'a>(
    record: &'a parser::NormalizedRecord,
    maximum: u64,
) -> Result<Cow<'a, str>, IngestError> {
    if !record.is_directory {
        return Ok(Cow::Borrowed(record.path.as_str()));
    }
    let capacity =
        record.path.len().checked_add(1).ok_or_else(|| {
            working_memory_error(u64::MAX, maximum, record.line, record.byte_offset)
        })?;
    let mut path = String::new();
    let capacity_bytes = u64::try_from(capacity).unwrap_or(u64::MAX);
    path.try_reserve_exact(capacity).map_err(|_| {
        working_memory_error(capacity_bytes, maximum, record.line, record.byte_offset)
    })?;
    path.push_str(&record.path);
    path.push('/');
    Ok(Cow::Owned(path))
}

fn parse_record_path(
    record: &parser::NormalizedRecord,
    options: &IngestOptions,
    maximum: u64,
) -> Result<RepositoryPath, IngestError> {
    let path = record_path_text(record, maximum)?;
    RepositoryPath::parse_with_limits(
        path.as_ref(),
        usize_limit(options.limits.max_path_bytes),
        usize_limit(options.limits.max_path_components),
    )
    .map_err(|error| {
        IngestError::new(
            ErrorCode::InvalidPath,
            record.line,
            record.byte_offset,
            "",
            error.to_string(),
        )
    })
}

fn check_index_memory(
    catalog_bytes: u64,
    event_capacity_bytes: u64,
    temporary_bytes: u64,
    maximum: u64,
    line: u64,
    byte_offset: u64,
) -> Result<(), IngestError> {
    let requested = catalog_bytes
        .checked_add(EVENTS_MEMORY_BASE)
        .and_then(|value| value.checked_add(event_capacity_bytes))
        .and_then(|value| value.checked_add(temporary_bytes))
        .ok_or_else(|| working_memory_error(u64::MAX, maximum, line, byte_offset))?;
    if requested > maximum {
        return Err(working_memory_error(requested, maximum, line, byte_offset));
    }
    Ok(())
}

fn catalog_memory_after_intern(
    catalog: &Catalog,
    catalog_bytes: u64,
    path: &RepositoryPath,
    contributor: &str,
    maximum: u64,
    line: u64,
    byte_offset: u64,
) -> Result<(u64, bool, bool), IngestError> {
    let path_is_new = !catalog
        .paths()
        .iter()
        .any(|candidate| candidate.canonical() == path.canonical());
    let contributor_is_new = !catalog
        .contributors()
        .iter()
        .any(|candidate| candidate == contributor);
    let mut bytes = catalog_bytes;
    if path_is_new {
        let path_bytes = catalog_path_bytes(path)
            .ok_or_else(|| working_memory_error(u64::MAX, maximum, line, byte_offset))?;
        bytes = bytes
            .checked_add(path_bytes)
            .and_then(|value| {
                value.checked_add(vector_growth_bytes(
                    catalog.paths().len().checked_add(1)?,
                    u64::try_from(size_of::<RepositoryPath>()).ok()?,
                )?)
            })
            .ok_or_else(|| working_memory_error(u64::MAX, maximum, line, byte_offset))?;
    }
    if contributor_is_new {
        let contributor_bytes = catalog_contributor_bytes(contributor)
            .ok_or_else(|| working_memory_error(u64::MAX, maximum, line, byte_offset))?;
        bytes = bytes
            .checked_add(contributor_bytes)
            .and_then(|value| {
                value.checked_add(vector_growth_bytes(
                    catalog.contributors().len().checked_add(1)?,
                    u64::try_from(size_of::<String>()).ok()?,
                )?)
            })
            .ok_or_else(|| working_memory_error(u64::MAX, maximum, line, byte_offset))?;
    }
    Ok((bytes, path_is_new, contributor_is_new))
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

fn intern_record(
    catalog: &mut Catalog,
    catalog_bytes: &mut u64,
    record: &parser::NormalizedRecord,
    _working_bytes: u64,
    options: &IngestOptions,
) -> Result<(), IngestError> {
    options.check_cancelled(record.line, record.byte_offset)?;
    let maximum = index_memory_limit(options);
    let temporary_bytes = record_path_memory_bytes(record)
        .ok_or_else(|| working_memory_error(u64::MAX, maximum, record.line, record.byte_offset))?;
    check_index_memory(
        *catalog_bytes,
        0,
        temporary_bytes,
        maximum,
        record.line,
        record.byte_offset,
    )?;
    let repository_path = parse_record_path(record, options, maximum)?;
    let (next_catalog_bytes, _, _) = catalog_memory_after_intern(
        catalog,
        *catalog_bytes,
        &repository_path,
        &record.username,
        maximum,
        record.line,
        record.byte_offset,
    )?;
    check_index_memory(
        next_catalog_bytes,
        0,
        temporary_bytes,
        maximum,
        record.line,
        record.byte_offset,
    )?;
    catalog.intern_path(&repository_path).map_err(|error| {
        IngestError::new(
            ErrorCode::InvalidPath,
            record.line,
            record.byte_offset,
            "",
            error.to_string(),
        )
    })?;
    catalog
        .intern_contributor(&record.username)
        .map_err(|error| {
            IngestError::new(
                ErrorCode::ContributorTooLong,
                record.line,
                record.byte_offset,
                "",
                error.to_string(),
            )
        })?;
    *catalog_bytes = next_catalog_bytes;
    Ok(())
}

fn build_index(
    mut parsed: parser::ParsedInput,
    options: &IngestOptions,
    mut catalog: Catalog,
    mut catalog_bytes: u64,
) -> Result<IndexedHistory, IngestError> {
    let input_bytes = parsed.input_bytes;
    let input_identity = InputIdentity {
        digest: parsed.input_hash,
        bytes: input_bytes,
    };
    options.report(ProgressUpdate {
        phase: ProgressPhase::Normalizing,
        bytes_read: input_bytes,
        input_bytes: Some(input_bytes),
        records_read: parsed.record_count,
    });
    options.check_cancelled(0, input_bytes)?;
    options.report(ProgressUpdate {
        phase: ProgressPhase::Sorting,
        bytes_read: input_bytes,
        input_bytes: Some(input_bytes),
        records_read: parsed.record_count,
    });

    let maximum = index_memory_limit(options);
    check_index_memory(catalog_bytes, 0, 0, maximum, 0, input_bytes)?;
    let mut events = Vec::new();
    parser::consume_sorted_records(&mut parsed, options, |record| {
        options.check_cancelled(record.line, record.byte_offset)?;
        let temporary_bytes = record_path_memory_bytes(&record).ok_or_else(|| {
            working_memory_error(u64::MAX, maximum, record.line, record.byte_offset)
        })?;
        let current_event_bytes = u64::try_from(events.capacity())
            .ok()
            .and_then(|capacity| capacity.checked_mul(size_of::<Event>() as u64))
            .ok_or_else(|| {
                working_memory_error(u64::MAX, maximum, record.line, record.byte_offset)
            })?;
        check_index_memory(
            catalog_bytes,
            current_event_bytes,
            temporary_bytes,
            maximum,
            record.line,
            record.byte_offset,
        )?;
        let repository_path = parse_record_path(&record, options, maximum)?;
        let (next_catalog_bytes, _, _) = catalog_memory_after_intern(
            &catalog,
            catalog_bytes,
            &repository_path,
            &record.username,
            maximum,
            record.line,
            record.byte_offset,
        )?;
        check_index_memory(
            next_catalog_bytes,
            current_event_bytes,
            temporary_bytes,
            maximum,
            record.line,
            record.byte_offset,
        )?;
        let path_id = catalog.intern_path(&repository_path).map_err(|error| {
            IngestError::new(
                ErrorCode::InvalidPath,
                record.line,
                record.byte_offset,
                "",
                error.to_string(),
            )
        })?;
        let contributor_id = catalog
            .intern_contributor(&record.username)
            .map_err(|error| {
                IngestError::new(
                    ErrorCode::ContributorTooLong,
                    record.line,
                    record.byte_offset,
                    "",
                    error.to_string(),
                )
            })?;
        catalog_bytes = next_catalog_bytes;
        let source_sequence = SourceSeq::new(record.source_sequence).ok_or_else(|| {
            IngestError::new(
                ErrorCode::EventCountLimit,
                record.line,
                record.byte_offset,
                "",
                "source sequence exceeds core identifier range",
            )
        })?;
        let action = match record.action {
            ParsedAction::Add => Action::Add,
            ParsedAction::Modify => Action::Modify,
            ParsedAction::Delete => Action::Delete,
        };
        let target = if record.is_directory {
            EventTarget::Directory(path_id)
        } else {
            EventTarget::File(path_id)
        };
        let color = record
            .colour
            .map(|colour| Rgb8::new(colour.0[0], colour.0[1], colour.0[2]));
        let old_capacity = events.capacity();
        let target_capacity = if events.len() == old_capacity {
            old_capacity.checked_add(1).ok_or_else(|| {
                working_memory_error(u64::MAX, maximum, record.line, record.byte_offset)
            })?
        } else {
            old_capacity
        };
        let target_event_bytes = u64::try_from(target_capacity)
            .ok()
            .and_then(|capacity| capacity.checked_mul(size_of::<Event>() as u64))
            .ok_or_else(|| {
                working_memory_error(u64::MAX, maximum, record.line, record.byte_offset)
            })?;
        check_index_memory(
            catalog_bytes,
            target_event_bytes,
            temporary_bytes,
            maximum,
            record.line,
            record.byte_offset,
        )?;
        if events.len() == old_capacity {
            events.try_reserve_exact(1).map_err(|_| {
                working_memory_error(
                    catalog_bytes
                        .checked_add(EVENTS_MEMORY_BASE)
                        .and_then(|value| value.checked_add(target_event_bytes))
                        .and_then(|value| value.checked_add(temporary_bytes))
                        .unwrap_or(u64::MAX),
                    maximum,
                    record.line,
                    record.byte_offset,
                )
            })?;
            let actual_event_bytes = u64::try_from(events.capacity())
                .ok()
                .and_then(|capacity| capacity.checked_mul(size_of::<Event>() as u64))
                .ok_or_else(|| {
                    working_memory_error(u64::MAX, maximum, record.line, record.byte_offset)
                })?;
            check_index_memory(
                catalog_bytes,
                actual_event_bytes,
                temporary_bytes,
                maximum,
                record.line,
                record.byte_offset,
            )?;
        }
        let event = Event::new(
            EventKey {
                timestamp: record.timestamp,
                source_sequence,
            },
            Generation::new(0).expect("zero is a valid generation"),
            contributor_id,
            target,
            action,
            color,
        );
        events.push(event);
        Ok(())
    })?;
    let dataset_identity = dataset_identity(&catalog, &events, input_identity);
    options.report(ProgressUpdate {
        phase: ProgressPhase::Publishing,
        bytes_read: input_bytes,
        input_bytes: Some(input_bytes),
        records_read: events.len() as u64,
    });
    Ok(IndexedHistory {
        catalog,
        events,
        input_identity,
        dataset_identity,
        input_bytes,
    })
}

fn usize_limit(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn dataset_identity(catalog: &Catalog, events: &[Event], input: InputIdentity) -> DatasetIdentity {
    let mut hasher = Hasher::new();
    hasher.update(b"gource-dataset-v1\0");
    hasher.update(&input.digest);
    for path in catalog.paths() {
        hasher.update(path.canonical().as_bytes());
        hasher.update(&[0]);
    }
    for contributor in catalog.contributors() {
        hasher.update(contributor.as_bytes());
        hasher.update(&[0]);
    }
    for event in events {
        hasher.update(&event.key.timestamp.to_le_bytes());
        hasher.update(&event.key.source_sequence.get().to_le_bytes());
        hasher.update(&event.contributor.get().to_le_bytes());
        hasher.update(&[match event.action {
            Action::Add => b'A',
            Action::Modify => b'M',
            Action::Delete => b'D',
        }]);
        match event.target {
            EventTarget::File(path) => {
                hasher.update(b"F");
                hasher.update(&path.get().to_le_bytes());
            }
            EventTarget::Directory(path) => {
                hasher.update(b"D");
                hasher.update(&path.get().to_le_bytes());
            }
        }
        if let Some(color) = event.color {
            hasher.update(&[1, color.r, color.g, color.b]);
        } else {
            hasher.update(&[0]);
        }
    }
    DatasetIdentity(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {

    use super::*;
    use gource_core::World;
    use std::fmt::Write as _;
    use std::fs;
    use std::path::Path;
    use std::process::{Command, Output};
    use tempfile::TempDir;

    fn git(repository: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git command")
    }

    fn repository() -> TempDir {
        let directory = tempfile::tempdir().expect("temporary repository");
        assert!(git(directory.path(), &["init", "--quiet"]).status.success());
        assert!(
            git(directory.path(), &["config", "user.name", "Test User"])
                .status
                .success()
        );
        assert!(
            git(
                directory.path(),
                &["config", "user.email", "test@example.invalid"]
            )
            .status
            .success()
        );
        directory
    }

    #[test]
    fn parse_input_dispatches_directory_to_git() {
        let directory = repository();
        fs::write(directory.path().join("tracked.txt"), b"content").expect("tracked file");
        assert!(
            git(directory.path(), &["add", "--", "tracked.txt"])
                .status
                .success()
        );
        let mut commit = Command::new("git");
        commit
            .arg("-C")
            .arg(directory.path())
            .args(["commit", "--quiet", "-m", "initial"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_DATE", "@100 +0000")
            .env("GIT_COMMITTER_DATE", "@100 +0000");
        assert!(commit.status().expect("git commit").success());

        let history = parse_input(InputSpec::file(directory.path()), IngestOptions::default())
            .expect("Git history");
        assert_eq!(history.len(), 1);
        assert!(
            history
                .catalog()
                .paths()
                .iter()
                .any(|path| path.canonical() == "tracked.txt")
        );
    }

    #[test]
    fn parse_path_keeps_regular_custom_log_behavior() {
        let directory = tempfile::tempdir().expect("temporary log directory");
        let path = directory.path().join("custom.log");
        fs::write(&path, b"42|custom-user|A|src/main.rs\n").expect("custom log");

        let history = parse_path(&path, IngestOptions::default()).expect("custom history");
        assert_eq!(history.len(), 1);
        assert_eq!(history.events()[0].timestamp(), 42);
        let repository_path = history
            .catalog()
            .path(history.events()[0].path_id())
            .expect("catalog path");
        assert_eq!(repository_path.canonical(), "src/main.rs");
    }

    #[test]
    fn input_identity_path_hashes_exact_file_bytes() {
        let directory = tempfile::tempdir().expect("temporary input directory");
        let path = directory.path().join("custom.log");
        let bytes = b"42|custom-user|A|src/main.rs\n";
        fs::write(&path, bytes).expect("custom log");

        let identity =
            input_identity_path(&path, &IngestOptions::default()).expect("file identity");
        assert_eq!(identity.bytes(), bytes.len() as u64);
        assert_eq!(identity.digest(), blake3::hash(bytes).as_bytes());
    }

    #[test]
    fn input_identity_path_rejects_bytes_beyond_the_configured_limit() {
        let directory = tempfile::tempdir().expect("temporary input directory");
        let path = directory.path().join("custom.log");
        fs::write(&path, b"012345").expect("custom log");
        let options =
            IngestOptions::default().with_limits(IngestLimits::default().with_max_input_bytes(5));

        let error = input_identity_path(&path, &options).expect_err("input limit");
        assert_eq!(error.code(), "input-too-large");
        assert_eq!(error.byte_offset, 5);
    }

    #[test]
    fn input_identity_path_checks_cancellation_between_chunks() {
        let directory = tempfile::tempdir().expect("temporary input directory");
        let path = directory.path().join("custom.log");
        fs::write(&path, b"012345").expect("custom log");
        let token = CancellationToken::new();
        token.cancel();
        let options = IngestOptions::default().with_cancellation(token);

        let error = input_identity_path(&path, &options).expect_err("cancelled input");
        assert!(error.is_cancelled());
        assert_eq!(error.byte_offset, 0);
    }

    #[test]
    fn external_runs_preserve_payload_associations_for_duplicates_and_equal_timestamps() {
        const PAIR_COUNT: usize = 15_000;
        const VARIANT_COUNT: usize = 1_024;
        let mut input = String::new();
        for pair in 0..PAIR_COUNT {
            let timestamp = PAIR_COUNT - pair;
            let path_variant = (pair * 7 + 3) % VARIANT_COUNT;
            let contributor_variant = (pair * 11 + 5) % VARIANT_COUNT;
            let action = match pair % 3 {
                0 => 'A',
                1 => 'M',
                _ => 'D',
            };
            let record = if pair % 2 == 0 {
                format!(
                    "{timestamp}|user-{contributor_variant}|{action}|payload/path-{path_variant}|#{:02x}{:02x}{:02x}",
                    pair % 256,
                    pair.wrapping_mul(3) % 256,
                    pair.wrapping_mul(7) % 256,
                )
            } else {
                format!(
                    "{timestamp}|user-{contributor_variant}|{action}|payload/path-{path_variant}"
                )
            };
            // Each pair is an exact duplicate, while the payload varies with
            // its source sequence.  This catches field/key reassociation
            // when equal-timestamp records cross external-sort runs.
            writeln!(&mut input, "{record}").expect("format input");
            writeln!(&mut input, "{record}").expect("format input");
        }
        let base_limits = IngestLimits::default().with_working_memory_bytes(6 * 1024 * 1024);
        let history = parse_bytes(input.as_bytes(), base_limits.clone().with_run_fan_in(2))
            .expect("bounded external sort");
        let alternate = parse_bytes(input.as_bytes(), base_limits.with_run_fan_in(3))
            .expect("alternate external merge");

        assert_eq!(history.len(), PAIR_COUNT * 2);
        assert_eq!(history.events(), alternate.events());
        assert!(
            history
                .events()
                .windows(2)
                .all(|events| events[0].key <= events[1].key)
        );
        for (index, event) in history.events().iter().enumerate() {
            let expected_source_sequence = ((PAIR_COUNT - 1 - index / 2) * 2) + index % 2;
            assert_eq!(
                event.source_sequence().get() as usize,
                expected_source_sequence
            );
            let pair = expected_source_sequence / 2;
            assert_eq!(event.timestamp(), (PAIR_COUNT - pair) as i64);

            let path_variant = (pair * 7 + 3) % VARIANT_COUNT;
            let expected_path = format!("payload/path-{path_variant}");
            assert_eq!(
                history
                    .catalog()
                    .path(event.path_id())
                    .expect("catalog path")
                    .canonical(),
                expected_path
            );
            let contributor_variant = (pair * 11 + 5) % VARIANT_COUNT;
            let expected_contributor = format!("user-{contributor_variant}");
            assert_eq!(
                history.catalog().contributor(event.contributor),
                Some(expected_contributor.as_str())
            );
            let expected_action = match pair % 3 {
                0 => Action::Add,
                1 => Action::Modify,
                _ => Action::Delete,
            };
            assert_eq!(event.action, expected_action);
            let expected_color = if pair.is_multiple_of(2) {
                Some(Rgb8::new(
                    (pair % 256) as u8,
                    (pair.wrapping_mul(3) % 256) as u8,
                    (pair.wrapping_mul(7) % 256) as u8,
                ))
            } else {
                None
            };
            assert_eq!(event.color, expected_color);
        }
        for duplicate in history.events().as_chunks::<2>().0 {
            assert_eq!(duplicate[0].timestamp(), duplicate[1].timestamp());
            assert_eq!(duplicate[0].path_id(), duplicate[1].path_id());
            assert_eq!(duplicate[0].contributor, duplicate[1].contributor);
            assert_eq!(duplicate[0].action, duplicate[1].action);
            assert_eq!(duplicate[0].color, duplicate[1].color);
        }
    }

    #[test]
    fn file_and_directory_targets_keep_distinct_catalog_paths() {
        let history = parse_bytes(
            b"0|user|A|dir\n1|user|D|dir/\n",
            IngestLimits::default()
                .with_max_record_bytes(256)
                .with_working_memory_bytes(64 * 1024),
        )
        .expect("file and directory targets");
        assert_eq!(history.catalog().path_count(), 2);
        let file_event = &history.events()[0];
        let directory_event = &history.events()[1];
        assert!(!file_event.is_directory());
        assert!(directory_event.is_directory());
        let file_path = history
            .catalog()
            .path(file_event.path_id())
            .expect("file path");
        let directory_path = history
            .catalog()
            .path(directory_event.path_id())
            .expect("directory path");
        assert_eq!(file_path.canonical(), "dir");
        assert!(!file_path.is_directory());
        assert_eq!(directory_path.canonical(), "dir/");
        assert!(directory_path.is_directory());

        let mut world = World::new();
        for event in history.events() {
            world
                .apply_event(event, history.catalog())
                .expect("target kind matches catalog path");
        }
        assert!(world.active_files().is_empty());
    }

    #[test]
    fn external_merge_event_reservation_respects_index_share() {
        let mut input = String::new();
        for index in 0..3_000u64 {
            writeln!(&mut input, "{}|user|A|path", 3_000 - index).expect("format input");
        }
        let limits = IngestLimits::default()
            .with_max_record_bytes(256)
            .with_working_memory_bytes(128 * 1024)
            .with_run_fan_in(2);
        let error = parse_bytes(input.as_bytes(), limits).expect_err("index memory limit");
        assert_eq!(error.code(), "working-memory-limit");
        assert!(
            error
                .message
                .contains(&parser::index_working_memory_bytes(128 * 1024).to_string())
        );
    }

    #[test]
    fn working_memory_limit_is_typed_and_no_prefix_is_published() {
        let mut input = String::new();
        for index in 0..128u64 {
            writeln!(&mut input, "{index}|user|A|path-{index}").expect("format input");
        }
        let limits = IngestLimits::default()
            .with_working_memory_bytes(16 * 1024)
            .with_working_disk_bytes(1 << 20);
        let error = parse_bytes(input.as_bytes(), limits).expect_err("memory limit");
        assert_eq!(error.code(), "working-memory-limit");
        assert!(error.message.contains("working_memory_bytes"));
        assert!(error.message.contains("8192"));
    }

    #[test]
    fn working_disk_limit_is_typed_during_spill() {
        let mut input = String::new();
        for index in 0..5_000u64 {
            writeln!(&mut input, "{index}|user|A|path").expect("format input");
        }
        let limits = IngestLimits::default()
            .with_max_record_bytes(256)
            .with_working_memory_bytes(16 * 1024)
            .with_working_disk_bytes(64);
        let error = parse_bytes(input.as_bytes(), limits).expect_err("disk limit");
        assert_eq!(error.code(), "working-disk-limit");
        assert!(error.message.contains("working_disk_bytes"));
        assert!(error.message.contains("64"));
    }
}
