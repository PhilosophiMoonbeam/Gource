// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! A bounded, private, content-addressed cache for complete indexed histories.
//!
//! The cache format is deliberately hand-written rather than delegated to a
//! general-purpose serializer.  Cache files are untrusted input: every length
//! is checked against the file and configured bounds before it is used, and a
//! complete history is built in temporary values before it is returned.

use blake3::Hasher;
use cap_std::ambient_authority;
#[cfg(any(unix, windows))]
use cap_std::fs::OpenOptionsExt;
use cap_std::fs::{Dir, File, Metadata as CapMetadata, OpenOptions};
#[cfg(unix)]
use cap_std::fs::{MetadataExt as CapMetadataExt, PermissionsExt};
use gource_core::{
    Action, CATALOG_SCHEMA_VERSION, Catalog, CatalogLimits, ContributorId, EVENT_SCHEMA_VERSION,
    Event, EventKey, EventTarget, Generation, HISTORY_SCHEMA_VERSION, History, PathId,
    RepositoryPath, Rgb8, SourceSeq,
};
use std::fs::{self};
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering as AtomicOrdering},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

use super::{DatasetIdentity, IndexedHistory, IngestOptions, InputIdentity};

/// Current on-disk cache format version.
pub const CACHE_SCHEMA_VERSION: u16 = 1;
/// Default aggregate cache capacity (4 GiB).
pub const DEFAULT_CACHE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Default maximum size of one cache entry.
pub const DEFAULT_CACHE_ENTRY_BYTES: u64 = DEFAULT_CACHE_BYTES;
/// Default maximum number of directory entries inspected during eviction.
pub const DEFAULT_CACHE_DIRECTORY_ENTRIES: u64 = 4096;
/// Default aggregate metadata budget for a cache directory scan.
pub const DEFAULT_CACHE_METADATA_BYTES: u64 = 16 * 1024 * 1024;

const CACHE_MAGIC: [u8; 8] = *b"GRCACHE\0";
const CHECKSUM_BYTES: usize = 32;
const HEADER_BYTES: usize = 224;
const EVENT_BYTES: usize = 46;
/// Minimum number of bytes needed for a structurally complete cache entry.
const MIN_CACHE_ENTRY_BYTES: u64 = (HEADER_BYTES + CHECKSUM_BYTES) as u64;
const LOCK_WAIT: Duration = Duration::from_secs(10);
const LOCK_RETRY: Duration = Duration::from_millis(2);
const LOCK_FILE: &str = ".cache.lock";
const ENTRY_SUFFIX: &str = ".bin";
const TEMP_SUFFIX: &str = ".tmp";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(windows)]
const WINDOWS_FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
#[cfg(windows)]
const WINDOWS_FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
const WINDOWS_FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// Cache configuration.  All resource limits are finite.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheConfig {
    /// Private directory containing cache entries.
    pub directory: PathBuf,
    /// Maximum aggregate size of published entry files.
    pub max_bytes: u64,
    /// Maximum size of one published entry file.
    pub max_entry_bytes: u64,
    /// Maximum number of directory entries inspected during eviction.
    pub max_directory_entries: u64,
    /// Maximum metadata bytes accounted for during an eviction scan.
    pub max_metadata_bytes: u64,
}

impl CacheConfig {
    /// Create a configuration using the default byte, entry, and metadata limits.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            max_bytes: DEFAULT_CACHE_BYTES,
            max_entry_bytes: DEFAULT_CACHE_ENTRY_BYTES,
            max_directory_entries: DEFAULT_CACHE_DIRECTORY_ENTRIES,
            max_metadata_bytes: DEFAULT_CACHE_METADATA_BYTES,
        }
    }

    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub fn with_max_entry_bytes(mut self, max_entry_bytes: u64) -> Self {
        self.max_entry_bytes = max_entry_bytes;
        self
    }

    pub fn with_max_directory_entries(mut self, max_directory_entries: u64) -> Self {
        self.max_directory_entries = max_directory_entries;
        self
    }

    pub fn with_max_metadata_bytes(mut self, max_metadata_bytes: u64) -> Self {
        self.max_metadata_bytes = max_metadata_bytes;
        self
    }

    /// Alias for callers that describe the aggregate limit as a capacity.
    pub fn with_capacity(self, max_bytes: u64) -> Self {
        self.with_max_bytes(max_bytes)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    pub fn max_entry_bytes(&self) -> u64 {
        self.max_entry_bytes
    }
    pub fn max_directory_entries(&self) -> u64 {
        self.max_directory_entries
    }

    pub fn max_metadata_bytes(&self) -> u64 {
        self.max_metadata_bytes
    }

    fn validate(&self) -> Result<(), CacheError> {
        if self.directory.as_os_str().is_empty() {
            return Err(CacheError::InvalidConfig("cache directory is empty"));
        }
        // u64::MAX is an accidental/unbounded sentinel, not a valid quota.
        if self.max_bytes == 0 || self.max_bytes == u64::MAX {
            return Err(CacheError::InvalidConfig(
                "max_bytes must be finite and non-zero",
            ));
        }
        if self.max_entry_bytes == 0 || self.max_entry_bytes == u64::MAX {
            return Err(CacheError::InvalidConfig(
                "max_entry_bytes must be finite and non-zero",
            ));
        }
        if self.max_entry_bytes > self.max_bytes {
            return Err(CacheError::InvalidConfig(
                "max_entry_bytes must not exceed max_bytes",
            ));
        }
        if self.max_directory_entries == 0 || self.max_directory_entries == u64::MAX {
            return Err(CacheError::InvalidConfig(
                "max_directory_entries must be finite and non-zero",
            ));
        }
        if self.max_metadata_bytes == 0 || self.max_metadata_bytes == u64::MAX {
            return Err(CacheError::InvalidConfig(
                "max_metadata_bytes must be finite and non-zero",
            ));
        }
        Ok(())
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self::new(PathBuf::from(".gource-cache"))
    }
}

impl From<PathBuf> for CacheConfig {
    fn from(directory: PathBuf) -> Self {
        Self::new(directory)
    }
}

impl From<&Path> for CacheConfig {
    fn from(directory: &Path) -> Self {
        Self::new(directory)
    }
}

impl From<&str> for CacheConfig {
    fn from(directory: &str) -> Self {
        Self::new(directory)
    }
}

impl From<String> for CacheConfig {
    fn from(directory: String) -> Self {
        Self::new(directory)
    }
}
impl From<&CacheConfig> for CacheConfig {
    fn from(config: &CacheConfig) -> Self {
        config.clone()
    }
}

/// A content-addressed cache key.
///
/// The key includes the exact finite input identity, the dataset-affecting
/// ingest options fingerprint, and this module's format/schema version.  UI,
/// cancellation, and progress state are intentionally not part of the key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CacheKey {
    input_identity: InputIdentity,
    options_fingerprint: [u8; 32],
    digest: [u8; 32],
    limits: CacheLimitEnvelope,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct CacheLimitEnvelope {
    max_path_bytes: u64,
    max_contributor_bytes: u64,
    max_path_components: u64,
    max_events: u64,
    working_memory_bytes: u64,
}

impl CacheKey {
    /// Build a key using all dataset-affecting finite ingest limits.
    pub fn new(input_identity: InputIdentity, options: &IngestOptions) -> Self {
        let limits = cache_limit_envelope(options);
        let options_fingerprint = options_fingerprint(options);
        let digest = key_digest(input_identity, options_fingerprint);
        Self {
            input_identity,
            options_fingerprint,
            digest,
            limits,
        }
    }

    /// Alias for [`Self::new`].
    pub fn from_options(input_identity: InputIdentity, options: &IngestOptions) -> Self {
        Self::new(input_identity, options)
    }

    /// Convenience constructor for a history that has already been indexed.
    pub fn from_history(history: &IndexedHistory, options: &IngestOptions) -> Self {
        Self::new(history.input_identity(), options)
    }

    /// Build a key from raw exact input bytes and finite ingest options.
    pub fn from_input_bytes(bytes: &[u8], options: &IngestOptions) -> Self {
        let digest = *blake3::hash(bytes).as_bytes();
        let input = InputIdentity {
            digest,
            bytes: bytes.len() as u64,
        };
        Self::new(input, options)
    }

    /// Build a key when an adapter already has the exact input digest/size.
    pub fn from_input_digest(digest: [u8; 32], bytes: u64, options: &IngestOptions) -> Self {
        Self::new(InputIdentity { digest, bytes }, options)
    }

    pub fn input_identity(self) -> InputIdentity {
        self.input_identity
    }

    pub fn options_fingerprint(self) -> [u8; 32] {
        self.options_fingerprint
    }

    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    /// Return the stable hexadecimal filename stem for this key.
    pub fn file_stem(&self) -> String {
        hex_digest(&self.digest)
    }
}

/// Errors that prevent opening or publishing a cache.  Malformed cache
/// entries are intentionally not surfaced as errors by [`EventCache::load`]:
/// they are removed and reported as misses.
#[derive(Debug, Error)]
pub enum CacheError {
    #[error("cache I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid cache configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("cache directory is not private: {0}")]
    InsecureDirectory(&'static str),
    #[error("cache lock was busy until the bounded wait expired")]
    LockTimeout,
    #[error("cache directory contains an unknown entry {name:?}")]
    UnknownEntry { name: String },
    #[error("cache directory entry count exceeds configured {limit}")]
    DirectoryEntryLimit { limit: u64 },
    #[error("cache directory metadata exceeds configured {limit} bytes")]
    DirectoryMetadataLimit { limit: u64 },
    #[error("cache entry is {size} bytes, exceeding the configured {maximum}-byte limit")]
    EntryTooLarge { size: u64, maximum: u64 },
    #[error("cache allocation failed within configured bounds")]
    Allocation,
    #[error("cache key input identity does not match the history")]
    KeyMismatch,
    #[error("history is not valid for cache publication: {0}")]
    InvalidHistory(&'static str),
}

#[derive(Clone)]
pub struct EventCache {
    config: CacheConfig,
    directory: Arc<Dir>,
    directory_identity: DirectoryIdentity,
}

impl std::fmt::Debug for EventCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EventCache")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirectoryIdentity {
    first: u64,
    second: u64,
}

impl EventCache {
    /// Open (and create if necessary) a private cache directory.
    pub fn open(config: impl Into<CacheConfig>) -> Result<Self, CacheError> {
        let config = config.into();
        config.validate()?;
        let (directory, identity) = ensure_private_directory(&config.directory)?;
        let directory = Arc::new(directory);
        let metadata = directory.dir_metadata()?;
        if !metadata.is_dir() || directory_identity(&directory, &metadata)? != identity {
            return Err(CacheError::InvalidConfig(
                "cache directory changed while opening",
            ));
        }
        Ok(Self {
            config,
            directory,
            directory_identity: identity,
        })
    }

    pub fn config(&self) -> &CacheConfig {
        &self.config
    }

    fn verify_directory(&self) -> Result<(), CacheError> {
        let metadata = self.directory.dir_metadata()?;
        if !metadata.is_dir()
            || directory_identity(&self.directory, &metadata)? != self.directory_identity
        {
            return Err(CacheError::InvalidConfig(
                "cache directory changed after opening",
            ));
        }
        if !directory_is_private(&self.directory, &metadata)? {
            return Err(CacheError::InsecureDirectory(
                "cache directory is no longer private",
            ));
        }
        Ok(())
    }

    pub fn load(&self, key: &CacheKey) -> Result<Option<IndexedHistory>, CacheError> {
        self.verify_directory()?;
        let _lock = CacheLock::acquire(&self.directory)?;
        self.verify_directory()?;
        let name = entry_name(key);
        let metadata = match self.directory.symlink_metadata(&name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() {
            remove_corrupt_entry(&self.directory, &name, None)?;
            return Ok(None);
        }
        let mut file = match open_entry_read(&self.directory, &name) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                if self
                    .directory
                    .symlink_metadata(&name)
                    .map(|metadata| metadata.file_type().is_symlink())
                    .unwrap_or(false)
                {
                    remove_corrupt_entry(&self.directory, &name, None)?;
                    return Ok(None);
                }
                return Err(error.into());
            }
        };
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            remove_corrupt_entry(&self.directory, &name, Some(&file))?;
            return Ok(None);
        }
        let size = metadata.len();
        if size > self.config.max_entry_bytes {
            remove_corrupt_entry(&self.directory, &name, Some(&file))?;
            return Ok(None);
        }
        let size_usize = match usize::try_from(size) {
            Ok(size) => size,
            Err(_) => {
                remove_corrupt_entry(&self.directory, &name, Some(&file))?;
                return Ok(None);
            }
        };
        let mut header = [0_u8; HEADER_BYTES];
        if let Err(error) = file.read_exact(&mut header) {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                remove_corrupt_entry(&self.directory, &name, Some(&file))?;
                return Ok(None);
            }
            return Err(error.into());
        }
        if parse_header(&header, key, size, self.config.max_entry_bytes).is_err() {
            remove_corrupt_entry(&self.directory, &name, Some(&file))?;
            return Ok(None);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size_usize)
            .map_err(|_| CacheError::Allocation)?;
        bytes.resize(size_usize, 0);
        bytes[..HEADER_BYTES].copy_from_slice(&header);
        if size_usize > HEADER_BYTES
            && let Err(error) = file.read_exact(&mut bytes[HEADER_BYTES..])
        {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                remove_corrupt_entry(&self.directory, &name, Some(&file))?;
                return Ok(None);
            }
            return Err(error.into());
        }
        let mut extra = [0_u8; 1];
        match file.read(&mut extra) {
            Ok(0) => {}
            Ok(_) => {
                remove_corrupt_entry(&self.directory, &name, Some(&file))?;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        }
        let decoded = match decode_entry(&bytes, key, self.config.max_entry_bytes) {
            Ok(history) => history,
            Err(DecodeError::Allocation) => return Err(CacheError::Allocation),
            Err(_) => {
                remove_corrupt_entry(&self.directory, &name, Some(&file))?;
                return Ok(None);
            }
        };
        self.verify_directory()?;
        Ok(Some(decoded))
    }

    pub fn store(&self, key: &CacheKey, history: &IndexedHistory) -> Result<(), CacheError> {
        self.verify_directory()?;
        if key.input_identity() != history.input_identity() {
            return Err(CacheError::KeyMismatch);
        }
        validate_history(history)?;
        let bytes = encode_entry(key, history, self.config.max_entry_bytes)?;
        let _lock = CacheLock::acquire(&self.directory)?;
        let incoming_size = u64::try_from(bytes.len()).map_err(|_| CacheError::EntryTooLarge {
            size: u64::MAX,
            maximum: self.config.max_bytes,
        })?;
        let target = entry_name(key);
        let old_target_size = existing_entry_size(&self.directory, &target)?;
        let quota_incoming =
            incoming_size
                .checked_add(old_target_size)
                .ok_or(CacheError::EntryTooLarge {
                    size: u64::MAX,
                    maximum: self.config.max_bytes,
                })?;
        evict_entries(
            &self.directory,
            self.config.max_bytes,
            self.config.max_entry_bytes,
            self.config.max_directory_entries,
            self.config.max_metadata_bytes,
            quota_incoming,
            Some(&target),
        )?;
        let (temporary_name, mut temporary) = create_private_temp(&self.directory, key)?;
        let write_result = (|| -> Result<(), CacheError> {
            temporary.write_all(&bytes)?;
            temporary.sync_all()?;
            drop(temporary);
            replace_entry(&self.directory, &temporary_name, &target)?;
            sync_directory(&self.directory)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = self.directory.remove_file(&temporary_name);
        }
        write_result?;
        self.verify_directory()?;
        Ok(())
    }

    #[cfg(test)]
    fn entry_path(&self, key: &CacheKey) -> PathBuf {
        self.config.directory.join(entry_name(key))
    }
}

fn entry_name(key: &CacheKey) -> String {
    format!("{}{}", key.file_stem(), ENTRY_SUFFIX)
}

fn cache_limit_envelope(options: &IngestOptions) -> CacheLimitEnvelope {
    let limits = &options.limits;
    CacheLimitEnvelope {
        max_path_bytes: limits.max_path_bytes,
        max_contributor_bytes: limits.max_contributor_bytes,
        max_path_components: limits.max_path_components,
        max_events: limits.max_events,
        working_memory_bytes: limits.working_memory_bytes,
    }
}

fn options_fingerprint(options: &IngestOptions) -> [u8; 32] {
    let limits = &options.limits;
    let mut hasher = Hasher::new();
    hasher.update(b"gource-cache-options-v1\0");
    for version in [
        CATALOG_SCHEMA_VERSION,
        EVENT_SCHEMA_VERSION,
        HISTORY_SCHEMA_VERSION,
    ] {
        hasher.update(&version.to_le_bytes());
    }
    for value in [
        limits.max_record_bytes,
        limits.max_input_bytes,
        limits.max_path_bytes,
        limits.max_contributor_bytes,
        limits.max_path_components,
        limits.max_events,
        limits.working_memory_bytes,
        limits.run_fan_in,
        limits.working_disk_bytes,
    ] {
        hasher.update(&value.to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn key_digest(input: InputIdentity, options_fingerprint: [u8; 32]) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(b"gource-cache-key-v1\0");
    hasher.update(&CACHE_SCHEMA_VERSION.to_le_bytes());
    hasher.update(input.digest());
    hasher.update(&input.bytes().to_le_bytes());
    hasher.update(&options_fingerprint);
    *hasher.finalize().as_bytes()
}

fn hex_digest(digest: &[u8; 32]) -> String {
    let mut result = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(result, "{byte:02x}");
    }
    result
}

fn ensure_private_directory(path: &Path) -> Result<(Dir, DirectoryIdentity), CacheError> {
    let name = path.file_name().ok_or(CacheError::InvalidConfig(
        "cache path must name a directory",
    ))?;
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    // Create the parent before opening a capability.  The final directory is
    // created with its private mode/ACL before it can become visible.
    Dir::create_ambient_dir_all(parent_path, ambient_authority())?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(CacheError::InvalidConfig(
                "cache directory must not be a symlink",
            ));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(CacheError::InvalidConfig("cache path is not a directory"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match create_private_directory(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error)
                    if error.kind() == io::ErrorKind::Unsupported
                        && cfg!(any(
                            target_os = "macos",
                            target_os = "freebsd",
                            target_os = "netbsd",
                            target_os = "openbsd",
                            target_os = "dragonfly",
                        )) =>
                {
                    return Err(CacheError::InsecureDirectory(
                        "private directory creation cannot guarantee an ACL-private root",
                    ));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(error) => return Err(error.into()),
    }

    let parent = Dir::open_ambient_dir(parent_path, ambient_authority())?;
    let child = Path::new(name);
    let metadata = match parent.symlink_metadata(child) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(CacheError::InvalidConfig(
                "cache directory disappeared while opening",
            ));
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        return Err(CacheError::InvalidConfig(
            "cache directory must not be a symlink",
        ));
    }
    if !metadata.is_dir() {
        return Err(CacheError::InvalidConfig("cache path is not a directory"));
    }

    // Open without following a final symlink, then inspect the metadata of
    // that held capability.  No pathname identity is trusted after this.
    let directory = open_child_directory(&parent, child)?;
    let held_metadata = directory.dir_metadata()?;
    if !held_metadata.is_dir() || held_metadata.is_symlink() {
        return Err(CacheError::InvalidConfig("cache path is not a directory"));
    }
    if !directory_is_private(&directory, &held_metadata)? {
        return Err(CacheError::InsecureDirectory(
            "existing cache directory is not private",
        ));
    }
    let identity = directory_identity(&directory, &held_metadata)?;
    Ok((directory, identity))
}

#[cfg(all(
    unix,
    not(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
    ))
))]
fn create_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(all(
    unix,
    any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
    )
))]
fn create_private_directory(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private directory creation cannot guarantee an ACL-private root",
    ))
}

#[cfg(windows)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let (descriptor, _) =
        windows_private_dacl().map_err(|error| io::Error::other(error.to_string()))?;
    let mut path_wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut attributes = WindowsSecurityAttributes {
        length: std::mem::size_of::<WindowsSecurityAttributes>() as u32,
        security_descriptor: descriptor,
        inherit_handle: 0,
    };
    let created = unsafe { CreateDirectoryW(path_wide.as_mut_ptr(), &mut attributes) };
    unsafe {
        let _ = LocalFree(descriptor);
    }
    if created != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(unix, windows)))]
fn create_private_directory(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private cache directories are unsupported on this platform",
    ))
}

#[cfg(unix)]
fn open_child_directory(parent: &Dir, child: &Path) -> io::Result<Dir> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(unix_directory_nofollow_flags());
    let file = parent.open_with(child, &options)?;
    Dir::reopen_dir(&file)
}

#[cfg(windows)]
fn open_child_directory(parent: &Dir, child: &Path) -> io::Result<Dir> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .custom_flags(WINDOWS_FILE_FLAG_BACKUP_SEMANTICS | WINDOWS_FILE_FLAG_OPEN_REPARSE_POINT);
    let file = parent.open_with(child, &options)?;
    Dir::reopen_dir(&file)
}

#[cfg(unix)]
fn directory_identity(
    directory: &Dir,
    metadata: &CapMetadata,
) -> Result<DirectoryIdentity, CacheError> {
    let _ = directory;
    Ok(DirectoryIdentity {
        first: metadata.dev(),
        second: metadata.ino(),
    })
}

#[cfg(windows)]
fn directory_identity(
    directory: &Dir,
    metadata: &CapMetadata,
) -> Result<DirectoryIdentity, CacheError> {
    let _ = metadata;
    use std::os::windows::io::AsRawHandle;
    let information = windows_file_information(directory.as_raw_handle())?;
    Ok(DirectoryIdentity {
        first: u64::from(information.volume_serial_number),
        second: (u64::from(information.file_index_high) << 32)
            | u64::from(information.file_index_low),
    })
}

#[cfg(not(any(unix, windows)))]
fn directory_identity(
    directory: &Dir,
    metadata: &CapMetadata,
) -> Result<DirectoryIdentity, CacheError> {
    let _ = directory;
    Ok(DirectoryIdentity {
        first: metadata.len(),
        second: metadata.len(),
    })
}
#[cfg(not(any(unix, windows)))]
fn open_child_directory(parent: &Dir, child: &Path) -> io::Result<Dir> {
    parent.open_dir(child)
}

#[cfg(unix)]
unsafe extern "C" {
    fn geteuid() -> u32;
}

#[cfg(unix)]
fn unix_directory_owner_matches(metadata: &CapMetadata) -> bool {
    metadata.uid() == unsafe { geteuid() }
}

#[cfg(unix)]
const fn unix_directory_nofollow_flags() -> i32 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        0o400000
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        0x100
    }
}

fn directory_is_private(directory: &Dir, metadata: &CapMetadata) -> Result<bool, CacheError> {
    #[cfg(all(unix, target_os = "macos",))]
    {
        if metadata.mode() & 0o077 != 0 || !unix_directory_owner_matches(metadata) {
            return Ok(false);
        }
        return macos_extended_acl_is_private(directory);
    }
    #[cfg(all(
        unix,
        any(
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly",
        )
    ))]
    {
        let _ = (directory, metadata);
        return Err(CacheError::InsecureDirectory(
            "BSD extended ACL privacy validation is unavailable",
        ));
    }
    #[cfg(all(
        unix,
        not(any(
            target_os = "macos",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly",
        ))
    ))]
    {
        let _ = directory;
        Ok(metadata.mode() & 0o077 == 0 && unix_directory_owner_matches(metadata))
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        return windows_directory_has_private_acl(directory);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (directory, metadata);
        return Err(CacheError::InsecureDirectory(
            "private cache directories are unsupported on this platform",
        ));
    }
}

#[cfg(unix)]
fn set_private_file_permissions(file: &File) -> Result<(), CacheError> {
    let mut permissions = file.metadata()?.permissions();
    permissions.set_mode(0o600);
    file.set_permissions(permissions)?;
    Ok(())
}

#[cfg(windows)]
fn set_private_file_permissions(file: &File) -> Result<(), CacheError> {
    set_windows_private_acl_handle(file)
}

#[cfg(not(any(unix, windows)))]
fn set_private_file_permissions(_file: &File) -> Result<(), CacheError> {
    Err(CacheError::InvalidConfig(
        "private cache files are unsupported on this platform",
    ))
}

#[cfg(windows)]
#[repr(C)]
struct WindowsByHandleFileInformation {
    file_attributes: u32,
    _creation_time: [u32; 2],
    _last_access_time: [u32; 2],
    _last_write_time: [u32; 2],
    volume_serial_number: u32,
    _file_size_high: u32,
    _file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}
#[cfg(windows)]
#[repr(C)]
struct WindowsSecurityAttributes {
    length: u32,
    security_descriptor: *mut std::ffi::c_void,
    inherit_handle: i32,
}

#[cfg(windows)]
#[repr(C)]
struct WindowsAclSizeInformation {
    ace_count: u32,
    acl_bytes_in_use: u32,
    acl_bytes_free: u32,
}

#[cfg(windows)]
#[repr(C)]
struct WindowsAceHeader {
    ace_type: u8,
    ace_flags: u8,
    ace_size: u16,
}

#[cfg(windows)]
#[repr(C)]
struct WindowsAccessAllowedAce {
    header: WindowsAceHeader,
    mask: u32,
    sid_start: u32,
}

#[cfg(windows)]
#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        descriptor: *const u16,
        revision: u32,
        security_descriptor: *mut *mut std::ffi::c_void,
        size: *mut u32,
    ) -> i32;
    fn GetSecurityDescriptorDacl(
        security_descriptor: *mut std::ffi::c_void,
        dacl_present: *mut i32,
        dacl: *mut *mut std::ffi::c_void,
        dacl_defaulted: *mut i32,
    ) -> i32;
    fn GetSecurityDescriptorControl(
        security_descriptor: *mut std::ffi::c_void,
        control: *mut u16,
        revision: *mut u32,
    ) -> i32;
    fn GetSecurityDescriptorOwner(
        security_descriptor: *mut std::ffi::c_void,
        owner: *mut *mut std::ffi::c_void,
        owner_defaulted: *mut i32,
    ) -> i32;
    fn GetSecurityInfo(
        handle: *mut std::ffi::c_void,
        object_type: u32,
        security_info: u32,
        owner: *mut *mut std::ffi::c_void,
        group: *mut *mut std::ffi::c_void,
        dacl: *mut *mut std::ffi::c_void,
        sacl: *mut *mut std::ffi::c_void,
        security_descriptor: *mut *mut std::ffi::c_void,
    ) -> u32;
    fn GetAclInformation(
        acl: *mut std::ffi::c_void,
        information: *mut std::ffi::c_void,
        information_length: u32,
        information_class: u32,
    ) -> i32;
    fn GetAce(acl: *mut std::ffi::c_void, ace_index: u32, ace: *mut *mut std::ffi::c_void) -> i32;
    fn GetLengthSid(sid: *mut std::ffi::c_void) -> u32;
    fn EqualSid(first_sid: *mut std::ffi::c_void, second_sid: *mut std::ffi::c_void) -> i32;
    fn SetSecurityInfo(
        handle: *mut std::ffi::c_void,
        object_type: u32,
        security_info: u32,
        owner: *mut std::ffi::c_void,
        group: *mut std::ffi::c_void,
        dacl: *mut std::ffi::c_void,
        sacl: *mut std::ffi::c_void,
    ) -> u32;
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LocalFree(memory: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn CreateDirectoryW(path: *mut u16, security_attributes: *mut WindowsSecurityAttributes)
    -> i32;
    fn GetFileInformationByHandle(
        file: *mut std::ffi::c_void,
        information: *mut WindowsByHandleFileInformation,
    ) -> i32;
    fn CloseHandle(file: *mut std::ffi::c_void) -> i32;
}

#[cfg(windows)]
fn windows_file_information(
    handle: *mut std::ffi::c_void,
) -> io::Result<WindowsByHandleFileInformation> {
    let mut information = unsafe { std::mem::zeroed() };
    let result = unsafe { GetFileInformationByHandle(handle, &mut information) };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(information)
    }
}
#[cfg(windows)]
#[repr(C)]
struct WindowsTokenOwner {
    owner: *mut std::ffi::c_void,
}

#[cfg(windows)]
struct WindowsOwnedHandle(*mut std::ffi::c_void);

#[cfg(windows)]
impl Drop for WindowsOwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
impl WindowsOwnedHandle {
    fn as_raw(&self) -> *mut std::ffi::c_void {
        self.0
    }
}

#[cfg(windows)]
#[repr(align(4))]
struct WindowsOwnerRightsSid([u8; 12]);

#[cfg(windows)]
const WINDOWS_OWNER_RIGHTS_SID: WindowsOwnerRightsSid =
    WindowsOwnerRightsSid([1, 1, 0, 0, 0, 0, 0, 3, 4, 0, 0, 0]);

#[cfg(windows)]
fn windows_owner_rights_sid_matches(sid: *mut std::ffi::c_void) -> bool {
    if sid.is_null() {
        return false;
    }
    unsafe {
        EqualSid(
            sid,
            WINDOWS_OWNER_RIGHTS_SID.0.as_ptr() as *mut std::ffi::c_void,
        ) != 0
    }
}

#[cfg(windows)]
fn windows_current_token() -> Result<WindowsOwnedHandle, CacheError> {
    const TOKEN_QUERY: u32 = 0x0008;
    const ERROR_NO_TOKEN: i32 = 1008;
    let mut token = std::ptr::null_mut();
    let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) };
    if opened == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_NO_TOKEN) {
            return Err(CacheError::InsecureDirectory(
                "unable to inspect the current Windows token",
            ));
        }
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if opened == 0 {
            return Err(CacheError::InsecureDirectory(
                "unable to inspect the current Windows token",
            ));
        }
    }
    if token.is_null() {
        return Err(CacheError::InsecureDirectory(
            "unable to inspect the current Windows token",
        ));
    }
    Ok(WindowsOwnedHandle(token))
}

#[cfg(windows)]
fn windows_current_token_owner_matches(owner: *mut std::ffi::c_void) -> Result<bool, CacheError> {
    const TOKEN_OWNER_INFORMATION: u32 = 4;
    let token = windows_current_token()?;
    let mut required = 0_u32;
    unsafe {
        let _ = GetTokenInformation(
            token.as_raw(),
            TOKEN_OWNER_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut required,
        );
    }
    if required < std::mem::size_of::<WindowsTokenOwner>() as u32 {
        return Err(CacheError::InsecureDirectory(
            "unable to inspect the current Windows token owner",
        ));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(required as usize)
        .map_err(|_| CacheError::InsecureDirectory("current Windows token owner is too large"))?;
    bytes.resize(required as usize, 0);
    let valid = unsafe {
        GetTokenInformation(
            token.as_raw(),
            TOKEN_OWNER_INFORMATION,
            bytes.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    };
    if valid == 0 {
        return Err(CacheError::InsecureDirectory(
            "unable to inspect the current Windows token owner",
        ));
    }
    let token_owner =
        unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const WindowsTokenOwner) };
    if token_owner.owner.is_null() {
        return Ok(false);
    }
    let buffer_start = bytes.as_ptr() as usize;
    let buffer_end = buffer_start
        .checked_add(bytes.len())
        .ok_or(CacheError::InsecureDirectory(
            "current Windows token owner is malformed",
        ))?;
    let sid_start = token_owner.owner as usize;
    let sid_header_end = sid_start
        .checked_add(8)
        .ok_or(CacheError::InsecureDirectory(
            "current Windows token owner is malformed",
        ))?;
    if sid_start < buffer_start || sid_header_end > buffer_end {
        return Err(CacheError::InsecureDirectory(
            "current Windows token owner is malformed",
        ));
    }
    let sid_length = unsafe { GetLengthSid(token_owner.owner) } as usize;
    let sid_end = sid_start
        .checked_add(sid_length)
        .ok_or(CacheError::InsecureDirectory(
            "current Windows token owner is malformed",
        ))?;
    if sid_length < 8 || sid_end > buffer_end {
        return Err(CacheError::InsecureDirectory(
            "current Windows token owner is malformed",
        ));
    }
    Ok(unsafe { EqualSid(owner, token_owner.owner) != 0 })
}

#[cfg(target_os = "macos")]
#[link(name = "c")]
unsafe extern "C" {
    fn acl_get_fd_np(file_descriptor: i32, acl_type: i32) -> *mut std::ffi::c_void;
    fn acl_free(acl: *mut std::ffi::c_void) -> i32;
}

#[cfg(target_os = "macos")]
fn macos_extended_acl_is_private(directory: &Dir) -> Result<bool, CacheError> {
    use std::os::unix::io::AsRawFd;
    const ACL_TYPE_EXTENDED: i32 = 0x0000_0100;
    let file = directory.try_clone()?.into_std_file();
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        if io::Error::last_os_error().raw_os_error() == Some(2) {
            return Ok(true);
        }
        return Err(CacheError::InsecureDirectory(
            "unable to inspect the macOS extended ACL",
        ));
    }
    unsafe {
        let _ = acl_free(acl);
    }
    Ok(false)
}

#[cfg(windows)]
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenThreadToken(
        thread: *mut std::ffi::c_void,
        desired_access: u32,
        open_as_self: i32,
        token: *mut *mut std::ffi::c_void,
    ) -> i32;
    fn OpenProcessToken(
        process: *mut std::ffi::c_void,
        desired_access: u32,
        token: *mut *mut std::ffi::c_void,
    ) -> i32;
    fn GetTokenInformation(
        token: *mut std::ffi::c_void,
        information_class: u32,
        information: *mut std::ffi::c_void,
        information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThread() -> *mut std::ffi::c_void;
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
}

#[cfg(windows)]
fn windows_private_dacl() -> Result<(*mut std::ffi::c_void, *mut std::ffi::c_void), CacheError> {
    use std::os::windows::ffi::OsStrExt;
    let descriptor_text: Vec<u16> = std::ffi::OsStr::new("D:P(A;;FA;;;OW)")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut descriptor = std::ptr::null_mut();
    let mut descriptor_size = 0_u32;
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor_text.as_ptr(),
            1,
            &mut descriptor,
            &mut descriptor_size,
        )
    };
    if converted == 0 {
        return Err(CacheError::Io(io::Error::last_os_error()));
    }
    let mut present = 0_i32;
    let mut dacl = std::ptr::null_mut();
    let mut defaulted = 0_i32;
    let valid =
        unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) };
    if valid == 0 || present == 0 || dacl.is_null() {
        unsafe {
            let _ = LocalFree(descriptor);
        }
        return Err(CacheError::Io(io::Error::last_os_error()));
    }
    // The DACL pointer remains valid until the descriptor is freed.
    Ok((descriptor, dacl))
}

#[cfg(windows)]
fn set_windows_private_acl_handle(file: &File) -> Result<(), CacheError> {
    use std::os::windows::io::AsRawHandle;
    let (descriptor, dacl) = windows_private_dacl()?;
    let status = unsafe {
        SetSecurityInfo(
            file.as_raw_handle(),
            1,
            0x8000_0004,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        )
    };
    unsafe {
        let _ = LocalFree(descriptor);
    }
    if status == 0 {
        Ok(())
    } else {
        Err(CacheError::Io(io::Error::from_raw_os_error(status as i32)))
    }
}

#[cfg(windows)]
fn windows_directory_has_private_acl(directory: &Dir) -> Result<bool, CacheError> {
    use std::os::windows::io::AsRawHandle;

    let file = directory.try_clone()?.into_std_file();
    let mut descriptor = std::ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            1,
            0x0000_0005,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(CacheError::Io(io::Error::from_raw_os_error(status as i32)));
    }
    if descriptor.is_null() {
        return Ok(false);
    }
    let result = (|| -> Result<bool, CacheError> {
        let mut control = 0_u16;
        let mut revision = 0_u32;
        let valid =
            unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
        if valid == 0 || control & 0x1000 == 0 {
            return Ok(false);
        }
        let mut present = 0_i32;
        let mut dacl = std::ptr::null_mut();
        let mut defaulted = 0_i32;
        let valid = unsafe {
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
        };
        if valid == 0 || present == 0 || dacl.is_null() {
            return Ok(false);
        }
        let mut owner = std::ptr::null_mut();
        let mut owner_defaulted = 0_i32;
        let valid =
            unsafe { GetSecurityDescriptorOwner(descriptor, &mut owner, &mut owner_defaulted) };
        if valid == 0 || owner.is_null() || !windows_current_token_owner_matches(owner)? {
            return Ok(false);
        }
        let mut size = WindowsAclSizeInformation {
            ace_count: 0,
            acl_bytes_in_use: 0,
            acl_bytes_free: 0,
        };
        let valid = unsafe {
            GetAclInformation(
                dacl,
                &mut size as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<WindowsAclSizeInformation>() as u32,
                2,
            )
        };
        // A private cache root has exactly one, non-inherited allow ACE.  The
        // exact count rejects group/everyone/anonymous and inherited grants.
        if valid == 0 || size.ace_count != 1 {
            return Ok(false);
        }
        let mut ace = std::ptr::null_mut();
        let valid = unsafe { GetAce(dacl, 0, &mut ace) };
        if valid == 0 || ace.is_null() {
            return Ok(false);
        }
        let allowed = unsafe { &*(ace as *const WindowsAccessAllowedAce) };
        if allowed.header.ace_type != 0
            || allowed.header.ace_flags != 0
            || allowed.header.ace_size != 20
            || allowed.mask != 0x001F_01FF
        {
            return Ok(false);
        }
        let ace_sid = unsafe { (ace as *mut u8).add(8) as *mut std::ffi::c_void };
        // Creation deliberately uses the OWNER RIGHTS well-known SID.  It
        // applies only to the descriptor owner, unlike the concrete owner SID
        // returned by GetSecurityDescriptorOwner.
        Ok(windows_owner_rights_sid_matches(ace_sid))
    })();
    unsafe {
        let _ = LocalFree(descriptor);
    }
    result
}

fn open_entry_read(directory: &Dir, name: &str) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options.custom_flags(WINDOWS_FILE_FLAG_OPEN_REPARSE_POINT);
    directory.open_with(name, &options)
}

#[cfg(windows)]
fn open_entry_identity(directory: &Dir, name: &str) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .access_mode(0)
        .custom_flags(WINDOWS_FILE_FLAG_BACKUP_SEMANTICS | WINDOWS_FILE_FLAG_OPEN_REPARSE_POINT);
    directory.open_with(name, &options)
}

fn open_lock_file(directory: &Dir) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options.custom_flags(unix_directory_nofollow_flags());
    #[cfg(windows)]
    options.custom_flags(WINDOWS_FILE_FLAG_OPEN_REPARSE_POINT);
    directory.open_with(LOCK_FILE, &options)
}

#[cfg(unix)]
unsafe extern "C" {
    fn flock(fd: std::os::raw::c_int, operation: std::os::raw::c_int) -> std::os::raw::c_int;
}

#[cfg(unix)]
fn try_advisory_lock(file: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    let result = unsafe { flock(file.as_raw_fd(), 2 | 4) };
    if result == 0 {
        Ok(true)
    } else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Ok(false)
        } else {
            Err(error)
        }
    }
}

#[cfg(unix)]
fn release_advisory_lock(file: &File) {
    use std::os::fd::AsRawFd;
    let _ = unsafe { flock(file.as_raw_fd(), 8) };
}

#[cfg(windows)]
#[repr(C)]
struct WindowsOverlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: *mut std::ffi::c_void,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LockFileEx(
        file: *mut std::ffi::c_void,
        flags: u32,
        reserved: u32,
        bytes_low: u32,
        bytes_high: u32,
        overlapped: *mut WindowsOverlapped,
    ) -> i32;
    fn UnlockFileEx(
        file: *mut std::ffi::c_void,
        reserved: u32,
        bytes_low: u32,
        bytes_high: u32,
        overlapped: *mut WindowsOverlapped,
    ) -> i32;
}

#[cfg(windows)]
fn try_advisory_lock(file: &File, overlapped: &mut WindowsOverlapped) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            0x0000_0002 | 0x0000_0001,
            0,
            1,
            0,
            overlapped,
        )
    };
    if result != 0 {
        Ok(true)
    } else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock || error.raw_os_error() == Some(33) {
            Ok(false)
        } else {
            Err(error)
        }
    }
}

#[cfg(windows)]
fn release_advisory_lock(file: &File, overlapped: &mut WindowsOverlapped) {
    use std::os::windows::io::AsRawHandle;
    let _ = unsafe { UnlockFileEx(file.as_raw_handle(), 0, 1, 0, overlapped) };
}

#[cfg(not(any(unix, windows)))]
fn try_advisory_lock(_file: &File) -> io::Result<bool> {
    Ok(true)
}

#[cfg(not(any(unix, windows)))]
fn release_advisory_lock(_file: &File) {}

#[cfg(unix)]
fn lock_has_single_link(metadata: &CapMetadata) -> bool {
    metadata.nlink() == 1
}

#[cfg(not(any(unix, windows)))]
fn lock_has_single_link(_metadata: &CapMetadata) -> bool {
    true
}

struct CacheLock {
    file: File,
    #[cfg(windows)]
    overlapped: WindowsOverlapped,
}

impl CacheLock {
    fn acquire(directory: &Dir) -> Result<Self, CacheError> {
        let file = open_lock_file(directory)?;
        let metadata = file.metadata()?;
        #[cfg(windows)]
        let file_information = {
            use std::os::windows::io::AsRawHandle;
            windows_file_information(file.as_raw_handle())?
        };
        #[cfg(windows)]
        if file_information.file_attributes & WINDOWS_FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(CacheError::InvalidConfig(
                "cache lock path must not be a symlink or reparse point",
            ));
        }
        if !metadata.file_type().is_file() {
            return Err(CacheError::InvalidConfig(
                "cache lock path is not a regular file",
            ));
        }
        #[cfg(windows)]
        let single_link = file_information.number_of_links == 1;
        #[cfg(not(windows))]
        let single_link = lock_has_single_link(&metadata);
        if !single_link {
            return Err(CacheError::InvalidConfig(
                "cache lock path must not be hard-linked",
            ));
        }
        set_private_file_permissions(&file)?;
        #[cfg(windows)]
        let mut overlapped = WindowsOverlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            event: std::ptr::null_mut(),
        };
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            #[cfg(windows)]
            let locked = try_advisory_lock(&file, &mut overlapped)?;
            #[cfg(not(windows))]
            let locked = try_advisory_lock(&file)?;
            if locked {
                #[cfg(windows)]
                {
                    return Ok(Self { file, overlapped });
                }
                #[cfg(not(windows))]
                {
                    return Ok(Self { file });
                }
            }
            if Instant::now() >= deadline {
                return Err(CacheError::LockTimeout);
            }
            thread::sleep(LOCK_RETRY);
        }
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        #[cfg(windows)]
        release_advisory_lock(&self.file, &mut self.overlapped);
        #[cfg(not(windows))]
        release_advisory_lock(&self.file);
    }
}

fn create_private_temp(directory: &Dir, key: &CacheKey) -> Result<(String, File), CacheError> {
    let stem = key.file_stem();
    let pid = std::process::id();
    for _ in 0..32_u8 {
        let counter = TEMP_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let name = format!(".{stem}.{pid}.{counter}{TEMP_SUFFIX}");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        match directory.open_with(&name, &options) {
            Ok(file) => {
                if let Err(error) = set_private_file_permissions(&file) {
                    let _ = directory.remove_file(&name);
                    return Err(error);
                }
                return Ok((name, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(CacheError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "unable to allocate a unique cache staging path",
    )))
}

fn replace_entry(directory: &Dir, temporary: &str, target: &str) -> Result<(), CacheError> {
    directory.rename(temporary, directory, target)?;
    Ok(())
}
fn sync_directory(directory: &Dir) -> Result<(), CacheError> {
    let mut options = OpenOptions::new();
    options.read(true);
    directory.open_with(".", &options)?.sync_all()?;
    Ok(())
}

fn remove_corrupt_entry(
    directory: &Dir,
    name: &str,
    expected: Option<&File>,
) -> Result<(), CacheError> {
    let metadata = match directory.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Ok(()),
    };
    if metadata.file_type().is_dir() {
        return Ok(());
    }
    if let Some(file) = expected {
        #[cfg(windows)]
        {
            let path_file = match open_entry_identity(directory, name) {
                Ok(file) => file,
                Err(_) => return Ok(()),
            };
            if !same_file_identity(&path_file, file) {
                return Ok(());
            }
        }
        #[cfg(not(windows))]
        {
            let opened = match file.metadata() {
                Ok(metadata) => metadata,
                Err(_) => return Ok(()),
            };
            if !same_file_identity(&metadata, &opened) {
                return Ok(());
            }
        }
    }
    let _ = directory.remove_file(name);
    Ok(())
}

#[cfg(unix)]
fn same_file_identity(path: &CapMetadata, opened: &CapMetadata) -> bool {
    path.dev() == opened.dev() && path.ino() == opened.ino()
}

#[cfg(windows)]
fn same_file_identity(path: &File, opened: &File) -> bool {
    use std::os::windows::io::AsRawHandle;
    let path = match windows_file_information(path.as_raw_handle()) {
        Ok(information) => information,
        Err(_) => return false,
    };
    let opened = match windows_file_information(opened.as_raw_handle()) {
        Ok(information) => information,
        Err(_) => return false,
    };
    path.volume_serial_number == opened.volume_serial_number
        && path.file_index_high == opened.file_index_high
        && path.file_index_low == opened.file_index_low
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(path: &CapMetadata, opened: &CapMetadata) -> bool {
    path.len() == opened.len()
}

fn existing_entry_size(directory: &Dir, name: &str) -> Result<u64, CacheError> {
    match directory.symlink_metadata(name) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(metadata.len()),
        Ok(_) => Ok(0),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug)]
struct EvictionEntry {
    modified: SystemTime,
    name: String,
    size: u64,
}

fn eviction_metadata_cost(name_bytes: usize, limit: u64) -> Result<u64, CacheError> {
    let fixed = u64::try_from(size_of::<EvictionEntry>())
        .ok()
        .and_then(|bytes| {
            u64::try_from(size_of::<CapMetadata>())
                .ok()
                .and_then(|metadata| bytes.checked_add(metadata))
        })
        .and_then(|bytes| {
            u64::try_from(name_bytes)
                .ok()
                .and_then(|name| bytes.checked_add(name))
        })
        .ok_or(CacheError::DirectoryMetadataLimit { limit })?;
    Ok(fixed)
}

fn remove_eviction_entry(directory: &Dir, name: impl AsRef<Path>) -> Result<bool, CacheError> {
    match directory.remove_file(name) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn evict_entries(
    directory: &Dir,
    maximum: u64,
    max_entry_bytes: u64,
    max_directory_entries: u64,
    max_metadata_bytes: u64,
    incoming_size: u64,
    replacing: Option<&str>,
) -> Result<(), CacheError> {
    let now = SystemTime::now();
    let mut entries = Vec::new();
    let mut total = incoming_size;
    let mut changed = false;
    let result = (|| -> Result<(), CacheError> {
        let mut directory_entries = 0_u64;
        let mut metadata_bytes = 0_u64;
        for item in directory.entries()? {
            let item = item?;
            directory_entries =
                directory_entries
                    .checked_add(1)
                    .ok_or(CacheError::DirectoryEntryLimit {
                        limit: max_directory_entries,
                    })?;
            if directory_entries > max_directory_entries {
                return Err(CacheError::DirectoryEntryLimit {
                    limit: max_directory_entries,
                });
            }

            let name = item.file_name();
            let name_text = name.to_string_lossy();
            let known = name_text == LOCK_FILE
                || name_text.ends_with(TEMP_SUFFIX)
                || name_text.ends_with(ENTRY_SUFFIX);
            if !known {
                return Err(CacheError::UnknownEntry {
                    name: name_text.into_owned(),
                });
            }
            let metadata_cost = eviction_metadata_cost(name_text.len(), max_metadata_bytes)?;
            metadata_bytes = metadata_bytes.checked_add(metadata_cost).ok_or(
                CacheError::DirectoryMetadataLimit {
                    limit: max_metadata_bytes,
                },
            )?;
            if metadata_bytes > max_metadata_bytes {
                return Err(CacheError::DirectoryMetadataLimit {
                    limit: max_metadata_bytes,
                });
            }

            if name_text == LOCK_FILE {
                continue;
            }
            if name_text.ends_with(TEMP_SUFFIX) {
                let temporary_metadata = match directory.symlink_metadata(&name) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if temporary_metadata.file_type().is_dir() {
                    continue;
                }
                if remove_eviction_entry(directory, &name)? {
                    changed = true;
                }
                continue;
            }

            let metadata = match directory.symlink_metadata(&name) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if !metadata.file_type().is_file() {
                if metadata.file_type().is_dir() {
                    continue;
                }
                if remove_eviction_entry(directory, &name)? {
                    changed = true;
                }
                continue;
            }
            if replacing.is_some_and(|target| target == name_text) {
                continue;
            }

            let size = metadata.len();
            if size < MIN_CACHE_ENTRY_BYTES || size > max_entry_bytes {
                if remove_eviction_entry(directory, &name)? {
                    changed = true;
                }
                continue;
            }
            total = total.checked_add(size).ok_or(CacheError::EntryTooLarge {
                size: u64::MAX,
                maximum,
            })?;
            let modified = metadata
                .modified()
                .map(|modified| modified.into_std())
                .unwrap_or(UNIX_EPOCH)
                .min(now);
            entries.try_reserve(1).map_err(|_| CacheError::Allocation)?;
            entries.push(EvictionEntry {
                modified,
                name: name_text.into_owned(),
                size,
            });
        }

        if total > maximum {
            entries.sort_by(|left, right| {
                left.modified
                    .cmp(&right.modified)
                    .then_with(|| left.name.cmp(&right.name))
            });
            for entry in entries {
                if total <= maximum {
                    break;
                }
                directory.remove_file(&entry.name)?;
                total = total.saturating_sub(entry.size);
                changed = true;
            }
        }
        if total > maximum {
            return Err(CacheError::InvalidConfig(
                "cache quota could not be enforced",
            ));
        }
        Ok(())
    })();
    if changed {
        sync_directory(directory)?;
    }
    result
}

fn validate_history(history: &IndexedHistory) -> Result<(), CacheError> {
    let catalog = history.catalog();
    let limits = &catalog.limits;
    if limits.path_bytes == 0
        || limits.contributor_bytes == 0
        || limits.path_components == 0
        || limits.max_paths == 0
        || limits.max_contributors == 0
    {
        return Err(CacheError::InvalidHistory("catalog limits are invalid"));
    }
    if catalog.version() != CATALOG_SCHEMA_VERSION {
        return Err(CacheError::InvalidHistory("unsupported catalog schema"));
    }
    if catalog.paths().len() > limits.max_paths {
        return Err(CacheError::InvalidHistory("catalog path limit exceeded"));
    }
    if catalog.contributors().len() > limits.max_contributors {
        return Err(CacheError::InvalidHistory(
            "catalog contributor limit exceeded",
        ));
    }
    for path in catalog.paths() {
        let reparsed = RepositoryPath::parse_with_limits(
            path.canonical(),
            limits.path_bytes,
            limits.path_components,
        )
        .map_err(|_| CacheError::InvalidHistory("catalog path invariant failed"))?;
        if &reparsed != path {
            return Err(CacheError::InvalidHistory("catalog path invariant failed"));
        }
    }
    for contributor in catalog.contributors() {
        if contributor.len() > limits.contributor_bytes {
            return Err(CacheError::InvalidHistory(
                "catalog contributor limit exceeded",
            ));
        }
    }
    for index in 1..history.events().len() {
        if history.events()[index - 1].key > history.events()[index].key {
            return Err(CacheError::InvalidHistory("event order is not canonical"));
        }
    }
    for event in history.events() {
        if event.version != EVENT_SCHEMA_VERSION {
            return Err(CacheError::InvalidHistory("unsupported event schema"));
        }
        if event.generation != Generation::ZERO {
            return Err(CacheError::InvalidHistory(
                "event generation is not the ingest generation",
            ));
        }
        if catalog.contributor(event.contributor).is_none() {
            return Err(CacheError::InvalidHistory("event contributor is unknown"));
        }
        let path = catalog
            .path(event.target.path_id())
            .ok_or(CacheError::InvalidHistory("event path is unknown"))?;
        if path.is_directory() != event.target.is_directory() {
            return Err(CacheError::InvalidHistory(
                "event target kind mismatches path",
            ));
        }
    }
    match source_sequence_bitmap(history.events()) {
        Ok(_) => {}
        Err(SequenceError::Allocation) => return Err(CacheError::Allocation),
        Err(SequenceError::Malformed) => {
            return Err(CacheError::InvalidHistory(
                "event source sequences are not a permutation",
            ));
        }
    }
    let expected = super::dataset_identity(catalog, history.events(), history.input_identity());
    if expected != history.dataset_identity() {
        return Err(CacheError::InvalidHistory(
            "dataset identity is inconsistent",
        ));
    }
    Ok(())
}

#[derive(Debug)]
enum SequenceError {
    Malformed,
    Allocation,
}

fn source_sequence_bitmap(events: &[Event]) -> Result<Vec<u64>, SequenceError> {
    let count = u64::try_from(events.len()).map_err(|_| SequenceError::Malformed)?;
    let words = count.checked_add(63).ok_or(SequenceError::Malformed)? / 64;
    let words = usize::try_from(words).map_err(|_| SequenceError::Malformed)?;
    let mut seen = Vec::new();
    seen.try_reserve_exact(words)
        .map_err(|_| SequenceError::Allocation)?;
    seen.resize(words, 0);
    for event in events {
        let sequence = event.source_sequence().get();
        if sequence >= count {
            return Err(SequenceError::Malformed);
        }
        let word = usize::try_from(sequence / 64).map_err(|_| SequenceError::Malformed)?;
        let bit = 1_u64 << (sequence % 64);
        if seen[word] & bit != 0 {
            return Err(SequenceError::Malformed);
        }
        seen[word] |= bit;
    }
    Ok(seen)
}
fn conservative_catalog_path_bytes(max_path_bytes: u64, max_path_components: u64) -> Option<u64> {
    let path_metadata = u64::try_from(size_of::<RepositoryPath>()).ok()?;
    let canonical_capacity = max_path_bytes.checked_mul(2)?;
    let component_bytes = max_path_bytes.checked_mul(2)?;
    let component_metadata = super::vector_capacity_bytes(
        max_path_components,
        u64::try_from(size_of::<String>()).ok()?,
    )?;
    let map_metadata = u64::try_from(size_of::<String>())
        .ok()?
        .checked_add(u64::try_from(size_of::<PathId>()).ok()?)?
        .checked_add(super::CATALOG_MAP_NODE_BYTES)?;
    path_metadata
        .checked_add(canonical_capacity)?
        .checked_add(component_bytes)?
        .checked_add(component_metadata)?
        .checked_add(canonical_capacity)?
        .checked_add(map_metadata)
}

fn conservative_catalog_contributor_bytes(max_contributor_bytes: u64) -> Option<u64> {
    max_contributor_bytes
        .checked_mul(2)?
        .checked_add(u64::try_from(size_of::<String>()).ok()?)?
        .checked_add(u64::try_from(size_of::<ContributorId>()).ok()?)?
        .checked_add(super::CATALOG_MAP_NODE_BYTES)
}

fn decoded_path_temporary_bound(raw_bytes: u64, max_components: u64) -> Option<u64> {
    let path_metadata = u64::try_from(size_of::<RepositoryPath>()).ok()?;
    let canonical_capacity = raw_bytes.checked_add(1)?.checked_mul(2)?;
    let component_bytes = raw_bytes.checked_mul(2)?;
    let component_metadata =
        super::vector_capacity_bytes(max_components, u64::try_from(size_of::<String>()).ok()?)?;
    path_metadata
        .checked_add(canonical_capacity)?
        .checked_add(component_bytes)?
        .checked_add(component_metadata)
}

fn decode_memory_check(
    catalog_bytes: u64,
    event_storage: u64,
    bitmap_storage: u64,
    encoded_bytes: u64,
    temporary_bytes: u64,
    maximum: u64,
) -> Result<(), DecodeError> {
    let requested = catalog_bytes
        .checked_add(super::EVENTS_MEMORY_BASE)
        .and_then(|value| value.checked_add(event_storage))
        .and_then(|value| value.checked_add(bitmap_storage))
        .and_then(|value| value.checked_add(encoded_bytes))
        .and_then(|value| value.checked_add(temporary_bytes))
        .ok_or(DecodeError::Malformed)?;
    if requested > maximum {
        Err(DecodeError::Malformed)
    } else {
        Ok(())
    }
}

fn decoded_memory_minimum_bound(
    encoded_bytes: u64,
    path_count: u64,
    contributor_count: u64,
    event_count: u64,
) -> Option<u64> {
    let path_storage =
        super::vector_capacity_bytes(path_count, u64::try_from(size_of::<RepositoryPath>()).ok()?)?;
    let contributor_storage =
        super::vector_capacity_bytes(contributor_count, u64::try_from(size_of::<String>()).ok()?)?;
    // Count the fixed map/node and minimum owned-string/component costs here;
    // actual lengths and capacities are accounted before each intern below.
    let path_items = conservative_catalog_path_bytes(1, 1)?.checked_mul(path_count)?;
    let contributor_items =
        conservative_catalog_contributor_bytes(1)?.checked_mul(contributor_count)?;
    let event_storage =
        super::vector_capacity_bytes(event_count, u64::try_from(size_of::<Event>()).ok()?)?;
    let words = event_count.checked_add(63)? / 64;
    let bitmap_storage = words.checked_mul(size_of::<u64>() as u64)?;
    super::CATALOG_MEMORY_BASE
        .checked_add(path_storage)?
        .checked_add(contributor_storage)?
        .checked_add(path_items)?
        .checked_add(contributor_items)?
        .checked_add(super::EVENTS_MEMORY_BASE)?
        .checked_add(event_storage)?
        .checked_add(bitmap_storage)?
        .checked_add(encoded_bytes)
}

fn indexed_history_memory_bound(catalog: &Catalog, event_count: u64) -> Option<u64> {
    let mut catalog_bytes = super::CATALOG_MEMORY_BASE;
    for (index, path) in catalog.paths().iter().enumerate() {
        catalog_bytes = catalog_bytes
            .checked_add(super::catalog_path_bytes(path)?)?
            .checked_add(super::vector_growth_bytes(
                index.checked_add(1)?,
                u64::try_from(size_of::<RepositoryPath>()).ok()?,
            )?)?;
    }
    for (index, contributor) in catalog.contributors().iter().enumerate() {
        catalog_bytes = catalog_bytes
            .checked_add(super::catalog_contributor_bytes(contributor)?)?
            .checked_add(super::vector_growth_bytes(
                index.checked_add(1)?,
                u64::try_from(size_of::<String>()).ok()?,
            )?)?;
    }
    let event_storage =
        super::vector_capacity_bytes(event_count, u64::try_from(size_of::<Event>()).ok()?)?;
    let words = event_count.checked_add(63)? / 64;
    catalog_bytes
        .checked_add(super::EVENTS_MEMORY_BASE)?
        .checked_add(event_storage)?
        .checked_add(words.checked_mul(size_of::<u64>() as u64)?)
}

fn validate_key_limits(
    key: &CacheKey,
    catalog: &Catalog,
    path_count: u64,
    contributor_count: u64,
    event_count: u64,
) -> Result<(), CacheError> {
    let envelope = key.limits;
    let limits = &catalog.limits;
    let catalog_path_bytes = u64::try_from(limits.path_bytes)
        .map_err(|_| CacheError::InvalidHistory("catalog path limit is not representable"))?;
    let catalog_contributor_bytes = u64::try_from(limits.contributor_bytes).map_err(|_| {
        CacheError::InvalidHistory("catalog contributor limit is not representable")
    })?;
    let catalog_components = u64::try_from(limits.path_components)
        .map_err(|_| CacheError::InvalidHistory("catalog component limit is not representable"))?;
    let catalog_max_paths = u64::try_from(limits.max_paths)
        .map_err(|_| CacheError::InvalidHistory("catalog path count limit is not representable"))?;
    let catalog_max_contributors = u64::try_from(limits.max_contributors).map_err(|_| {
        CacheError::InvalidHistory("catalog contributor count limit is not representable")
    })?;
    if envelope.max_path_bytes == 0
        || envelope.max_contributor_bytes == 0
        || envelope.max_path_components == 0
        || envelope.max_events == 0
        || envelope.working_memory_bytes == 0
        || catalog_path_bytes > envelope.max_path_bytes
        || catalog_contributor_bytes > envelope.max_contributor_bytes
        || catalog_components > envelope.max_path_components
        || catalog_max_paths > envelope.max_events
        || catalog_max_contributors > envelope.max_events
        || path_count > envelope.max_events
        || contributor_count > envelope.max_events
        || event_count > envelope.max_events
    {
        return Err(CacheError::InvalidHistory(
            "cache entry exceeds caller ingest limits",
        ));
    }
    let bound = indexed_history_memory_bound(catalog, event_count).ok_or(
        CacheError::InvalidHistory("decoded size arithmetic overflow"),
    )?;
    if bound > envelope.working_memory_bytes {
        return Err(CacheError::InvalidHistory(
            "decoded cache entry exceeds caller working-memory limit",
        ));
    }
    Ok(())
}

fn encode_entry(
    key: &CacheKey,
    history: &IndexedHistory,
    maximum: u64,
) -> Result<Vec<u8>, CacheError> {
    let catalog = history.catalog();
    let limits = &catalog.limits;
    let path_count =
        u64::try_from(catalog.paths().len()).map_err(|_| CacheError::EntryTooLarge {
            size: u64::MAX,
            maximum,
        })?;
    let contributor_count =
        u64::try_from(catalog.contributors().len()).map_err(|_| CacheError::EntryTooLarge {
            size: u64::MAX,
            maximum,
        })?;
    let event_count =
        u64::try_from(history.events().len()).map_err(|_| CacheError::EntryTooLarge {
            size: u64::MAX,
            maximum,
        })?;
    let payload_bytes = encoded_payload_size(catalog, history.events())?;
    let total_bytes = (HEADER_BYTES as u64)
        .checked_add(payload_bytes)
        .and_then(|value| value.checked_add(CHECKSUM_BYTES as u64))
        .ok_or(CacheError::EntryTooLarge {
            size: u64::MAX,
            maximum,
        })?;
    validate_key_limits(key, catalog, path_count, contributor_count, event_count)?;
    if total_bytes > maximum {
        return Err(CacheError::EntryTooLarge {
            size: total_bytes,
            maximum,
        });
    }
    let total_usize = usize::try_from(total_bytes).map_err(|_| CacheError::Allocation)?;
    let path_bytes_limit =
        u64::try_from(limits.path_bytes).map_err(|_| CacheError::EntryTooLarge {
            size: u64::MAX,
            maximum,
        })?;
    let contributor_bytes_limit =
        u64::try_from(limits.contributor_bytes).map_err(|_| CacheError::EntryTooLarge {
            size: u64::MAX,
            maximum,
        })?;
    let path_components_limit =
        u64::try_from(limits.path_components).map_err(|_| CacheError::EntryTooLarge {
            size: u64::MAX,
            maximum,
        })?;
    let max_paths_limit =
        u64::try_from(limits.max_paths).map_err(|_| CacheError::EntryTooLarge {
            size: u64::MAX,
            maximum,
        })?;
    let max_contributors_limit =
        u64::try_from(limits.max_contributors).map_err(|_| CacheError::EntryTooLarge {
            size: u64::MAX,
            maximum,
        })?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(total_usize)
        .map_err(|_| CacheError::Allocation)?;
    output.extend_from_slice(&CACHE_MAGIC);
    put_u16(&mut output, CACHE_SCHEMA_VERSION);
    put_u16(&mut output, 0);
    put_u32(&mut output, HEADER_BYTES as u32);
    put_u64(&mut output, payload_bytes);
    output.extend_from_slice(key.digest());
    output.extend_from_slice(history.input_identity().digest());
    put_u64(&mut output, history.input_identity().bytes());
    output.extend_from_slice(&key.options_fingerprint());
    output.extend_from_slice(history.dataset_identity().as_bytes());
    put_u64(&mut output, path_bytes_limit);
    put_u64(&mut output, contributor_bytes_limit);
    put_u64(&mut output, path_components_limit);
    put_u64(&mut output, max_paths_limit);
    put_u64(&mut output, max_contributors_limit);
    put_u64(&mut output, path_count);
    put_u64(&mut output, contributor_count);
    put_u64(&mut output, event_count);

    for path in catalog.paths() {
        put_bytes(&mut output, path.as_bytes());
    }
    for contributor in catalog.contributors() {
        put_bytes(&mut output, contributor.as_bytes());
    }
    for event in history.events() {
        put_i64(&mut output, event.key.timestamp);
        put_u64(&mut output, event.key.source_sequence.get());
        put_u64(&mut output, event.generation.get());
        put_u64(&mut output, event.contributor.as_u64());
        match event.target {
            EventTarget::File(path) => {
                output.push(0);
                put_u64(&mut output, path.get() as u64);
            }
            EventTarget::Directory(path) => {
                output.push(1);
                put_u64(&mut output, path.get() as u64);
            }
        }
        output.push(match event.action {
            Action::Add => b'A',
            Action::Modify => b'M',
            Action::Delete => b'D',
        });
        match event.color {
            Some(color) => {
                output.push(1);
                output.extend_from_slice(&color.as_array());
            }
            None => {
                output.push(0);
                output.extend_from_slice(&[0, 0, 0]);
            }
        }
    }
    debug_assert_eq!(output.len(), total_usize - CHECKSUM_BYTES);
    let checksum = blake3::hash(&output);
    output.extend_from_slice(checksum.as_bytes());
    Ok(output)
}

fn encoded_payload_size(catalog: &Catalog, events: &[Event]) -> Result<u64, CacheError> {
    let mut size = 0_u64;
    for path in catalog.paths() {
        let bytes =
            u64::try_from(path.as_bytes().len()).map_err(|_| CacheError::EntryTooLarge {
                size: u64::MAX,
                maximum: u64::MAX,
            })?;
        size = checked_size_add(
            size,
            bytes.checked_add(8).ok_or(CacheError::EntryTooLarge {
                size: u64::MAX,
                maximum: u64::MAX,
            })?,
        )?;
    }
    for contributor in catalog.contributors() {
        let bytes = u64::try_from(contributor.len()).map_err(|_| CacheError::EntryTooLarge {
            size: u64::MAX,
            maximum: u64::MAX,
        })?;
        size = checked_size_add(
            size,
            bytes.checked_add(8).ok_or(CacheError::EntryTooLarge {
                size: u64::MAX,
                maximum: u64::MAX,
            })?,
        )?;
    }
    let event_bytes = (events.len() as u64)
        .checked_mul(EVENT_BYTES as u64)
        .ok_or(CacheError::EntryTooLarge {
            size: u64::MAX,
            maximum: u64::MAX,
        })?;
    checked_size_add(size, event_bytes)
}

fn checked_size_add(left: u64, right: u64) -> Result<u64, CacheError> {
    left.checked_add(right).ok_or(CacheError::EntryTooLarge {
        size: u64::MAX,
        maximum: u64::MAX,
    })
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_i64(output: &mut Vec<u8>, value: i64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes(output: &mut Vec<u8>, value: &[u8]) {
    put_u64(output, value.len() as u64);
    output.extend_from_slice(value);
}

#[derive(Debug)]
enum DecodeError {
    Malformed,
    Allocation,
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.position)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(DecodeError::Malformed)?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or(DecodeError::Malformed)?;
        self.position = end;
        Ok(bytes)
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(self.u64()? as i64)
    }

    fn array32(&mut self) -> Result<[u8; 32], DecodeError> {
        let bytes = self.take(32)?;
        let mut output = [0_u8; 32];
        output.copy_from_slice(bytes);
        Ok(output)
    }

    fn bounded_bytes(&mut self, maximum: u64) -> Result<&'a [u8], DecodeError> {
        let length = self.u64()?;
        if length > maximum || length > self.remaining() as u64 {
            return Err(DecodeError::Malformed);
        }
        let length = usize::try_from(length).map_err(|_| DecodeError::Malformed)?;
        self.take(length)
    }
}
#[derive(Clone)]
struct HeaderMetadata {
    payload_bytes: u64,
    input_digest: [u8; 32],
    input_bytes: u64,
    dataset_digest: [u8; 32],
    limits: CatalogLimits,
    path_count: u64,
    contributor_count: u64,
    event_count: u64,
}

fn parse_header(
    header_bytes: &[u8],
    key: &CacheKey,
    total_bytes: u64,
    maximum: u64,
) -> Result<HeaderMetadata, DecodeError> {
    if header_bytes.len() != HEADER_BYTES
        || total_bytes < (HEADER_BYTES + CHECKSUM_BYTES) as u64
        || total_bytes > maximum
    {
        return Err(DecodeError::Malformed);
    }
    let mut header = Reader::new(header_bytes);
    if header.take(CACHE_MAGIC.len())? != CACHE_MAGIC {
        return Err(DecodeError::Malformed);
    }
    if header.u16()? != CACHE_SCHEMA_VERSION || header.u16()? != 0 {
        return Err(DecodeError::Malformed);
    }
    if header.u32()? as usize != HEADER_BYTES {
        return Err(DecodeError::Malformed);
    }
    let payload_bytes = header.u64()?;
    let expected_total = (HEADER_BYTES as u64)
        .checked_add(payload_bytes)
        .and_then(|value| value.checked_add(CHECKSUM_BYTES as u64))
        .ok_or(DecodeError::Malformed)?;
    if expected_total != total_bytes || payload_bytes > maximum {
        return Err(DecodeError::Malformed);
    }
    let encoded_key = header.array32()?;
    if encoded_key != *key.digest() {
        return Err(DecodeError::Malformed);
    }
    let input_digest = header.array32()?;
    let input_bytes = header.u64()?;
    if input_digest != *key.input_identity().digest() || input_bytes != key.input_identity().bytes()
    {
        return Err(DecodeError::Malformed);
    }
    let encoded_options = header.array32()?;
    if encoded_options != key.options_fingerprint() {
        return Err(DecodeError::Malformed);
    }
    let dataset_digest = header.array32()?;
    let path_bytes_raw = header.u64()?;
    let contributor_bytes_raw = header.u64()?;
    let path_components_raw = header.u64()?;
    let max_paths_raw = header.u64()?;
    let max_contributors_raw = header.u64()?;
    let path_count = header.u64()?;
    let contributor_count = header.u64()?;
    let event_count = header.u64()?;
    let limits = CatalogLimits {
        path_bytes: usize::try_from(path_bytes_raw).map_err(|_| DecodeError::Malformed)?,
        contributor_bytes: usize::try_from(contributor_bytes_raw)
            .map_err(|_| DecodeError::Malformed)?,
        path_components: usize::try_from(path_components_raw)
            .map_err(|_| DecodeError::Malformed)?,
        max_paths: usize::try_from(max_paths_raw).map_err(|_| DecodeError::Malformed)?,
        max_contributors: usize::try_from(max_contributors_raw)
            .map_err(|_| DecodeError::Malformed)?,
    };
    if header.remaining() != 0
        || path_bytes_raw == 0
        || contributor_bytes_raw == 0
        || path_components_raw == 0
        || max_paths_raw == 0
        || max_contributors_raw == 0
        || path_count > max_paths_raw
        || contributor_count > max_contributors_raw
        || path_count > usize::MAX as u64
        || contributor_count > usize::MAX as u64
        || event_count > usize::MAX as u64
    {
        return Err(DecodeError::Malformed);
    }
    let envelope = key.limits;
    if path_bytes_raw > envelope.max_path_bytes
        || contributor_bytes_raw > envelope.max_contributor_bytes
        || path_components_raw > envelope.max_path_components
        || max_paths_raw > envelope.max_events
        || max_contributors_raw > envelope.max_events
        || path_count > envelope.max_events
        || contributor_count > envelope.max_events
        || event_count > envelope.max_events
        || envelope.working_memory_bytes == 0
        || decoded_memory_minimum_bound(total_bytes, path_count, contributor_count, event_count)
            .is_none_or(|bound| bound > envelope.working_memory_bytes)
    {
        return Err(DecodeError::Malformed);
    }
    Ok(HeaderMetadata {
        payload_bytes,
        input_digest,
        input_bytes,
        dataset_digest,
        limits,
        path_count,
        contributor_count,
        event_count,
    })
}

fn decode_entry(bytes: &[u8], key: &CacheKey, maximum: u64) -> Result<IndexedHistory, DecodeError> {
    if bytes.len() < HEADER_BYTES + CHECKSUM_BYTES || bytes.len() as u64 > maximum {
        return Err(DecodeError::Malformed);
    }
    let checksum_position = bytes.len() - CHECKSUM_BYTES;
    let expected_checksum = blake3::hash(&bytes[..checksum_position]);
    if expected_checksum.as_bytes() != &bytes[checksum_position..] {
        return Err(DecodeError::Malformed);
    }
    let header_metadata = parse_header(&bytes[..HEADER_BYTES], key, bytes.len() as u64, maximum)?;
    let _payload_bytes = header_metadata.payload_bytes;
    let input_digest = header_metadata.input_digest;
    let input_bytes = header_metadata.input_bytes;
    let dataset_digest = header_metadata.dataset_digest;
    let limits = header_metadata.limits;
    let mut catalog = Catalog::with_limits(limits.clone());
    let path_count = header_metadata.path_count;
    let contributor_count = header_metadata.contributor_count;
    let event_count = header_metadata.event_count;
    let mut payload = Reader::new(&bytes[HEADER_BYTES..checksum_position]);
    let path_bytes = limits.path_bytes;
    let contributor_bytes = limits.contributor_bytes;
    let path_components = limits.path_components;
    let encoded_bytes = u64::try_from(bytes.len()).map_err(|_| DecodeError::Malformed)?;
    let event_storage = super::vector_capacity_bytes(
        event_count,
        u64::try_from(size_of::<Event>()).map_err(|_| DecodeError::Malformed)?,
    )
    .ok_or(DecodeError::Malformed)?;
    let bitmap_storage = event_count
        .checked_add(63)
        .ok_or(DecodeError::Malformed)?
        .checked_div(64)
        .ok_or(DecodeError::Malformed)?
        .checked_mul(size_of::<u64>() as u64)
        .ok_or(DecodeError::Malformed)?;
    let mut catalog_bytes = super::CATALOG_MEMORY_BASE;
    let path_components_u64 = u64::try_from(path_components).map_err(|_| DecodeError::Malformed)?;
    let path_count_usize = path_count as usize;
    if path_count_usize > payload.remaining() / 8 {
        return Err(DecodeError::Malformed);
    }
    for _ in 0..path_count_usize {
        let raw = payload.bounded_bytes(path_bytes as u64)?;
        let mut component_input = raw;
        if component_input.first() == Some(&b'/') {
            component_input = &component_input[1..];
        }
        if component_input.last() == Some(&b'/') {
            component_input = &component_input[..component_input.len() - 1];
        }
        let raw_component_count = if component_input.is_empty() {
            0
        } else {
            u64::try_from(
                component_input
                    .iter()
                    .filter(|&&byte| byte == b'/')
                    .count()
                    .checked_add(1)
                    .ok_or(DecodeError::Malformed)?,
            )
            .map_err(|_| DecodeError::Malformed)?
        };
        if raw_component_count > path_components_u64 {
            return Err(DecodeError::Malformed);
        }
        let temporary_components = raw_component_count;
        let temporary_bytes = decoded_path_temporary_bound(raw.len() as u64, temporary_components)
            .ok_or(DecodeError::Malformed)?;
        decode_memory_check(
            catalog_bytes,
            event_storage,
            bitmap_storage,
            encoded_bytes,
            temporary_bytes,
            key.limits.working_memory_bytes,
        )?;
        let value = std::str::from_utf8(raw).map_err(|_| DecodeError::Malformed)?;
        let path = RepositoryPath::parse_with_limits(value, path_bytes, path_components)
            .map_err(|_| DecodeError::Malformed)?;
        let path_count_before = catalog.paths().len();
        let next_catalog_bytes = catalog_bytes
            .checked_add(super::catalog_path_bytes(&path).ok_or(DecodeError::Malformed)?)
            .and_then(|value| {
                value.checked_add(super::vector_growth_bytes(
                    path_count_before.checked_add(1)?,
                    u64::try_from(size_of::<RepositoryPath>()).ok()?,
                )?)
            })
            .ok_or(DecodeError::Malformed)?;
        decode_memory_check(
            next_catalog_bytes,
            event_storage,
            bitmap_storage,
            encoded_bytes,
            temporary_bytes,
            key.limits.working_memory_bytes,
        )?;
        catalog
            .intern_path(&path)
            .map_err(|_| DecodeError::Malformed)?;
        if catalog.paths().len()
            != path_count_before
                .checked_add(1)
                .ok_or(DecodeError::Malformed)?
        {
            return Err(DecodeError::Malformed);
        }
        catalog_bytes = next_catalog_bytes;
    }

    let contributor_count_usize = contributor_count as usize;
    if contributor_count_usize > payload.remaining() / 8 {
        return Err(DecodeError::Malformed);
    }
    for _ in 0..contributor_count_usize {
        let raw = payload.bounded_bytes(contributor_bytes as u64)?;
        let value = std::str::from_utf8(raw).map_err(|_| DecodeError::Malformed)?;
        let contributor_count_before = catalog.contributors().len();
        let next_catalog_bytes = catalog_bytes
            .checked_add(super::catalog_contributor_bytes(value).ok_or(DecodeError::Malformed)?)
            .and_then(|value| {
                value.checked_add(super::vector_growth_bytes(
                    contributor_count_before.checked_add(1)?,
                    u64::try_from(size_of::<String>()).ok()?,
                )?)
            })
            .ok_or(DecodeError::Malformed)?;
        decode_memory_check(
            next_catalog_bytes,
            event_storage,
            bitmap_storage,
            encoded_bytes,
            0,
            key.limits.working_memory_bytes,
        )?;
        catalog
            .intern_contributor(value)
            .map_err(|_| DecodeError::Malformed)?;
        if catalog.contributors().len()
            != contributor_count_before
                .checked_add(1)
                .ok_or(DecodeError::Malformed)?
        {
            return Err(DecodeError::Malformed);
        }
        catalog_bytes = next_catalog_bytes;
    }

    decode_memory_check(
        catalog_bytes,
        event_storage,
        bitmap_storage,
        encoded_bytes,
        0,
        key.limits.working_memory_bytes,
    )?;
    let event_count_usize = event_count as usize;
    let required_event_bytes = event_count
        .checked_mul(EVENT_BYTES as u64)
        .ok_or(DecodeError::Malformed)?;
    if required_event_bytes > payload.remaining() as u64 {
        return Err(DecodeError::Malformed);
    }
    let mut events = Vec::new();
    events
        .try_reserve_exact(event_count_usize)
        .map_err(|_| DecodeError::Allocation)?;
    for _ in 0..event_count_usize {
        let timestamp = payload.i64()?;
        let source_sequence = SourceSeq::new(payload.u64()?).ok_or(DecodeError::Malformed)?;
        let generation_value = payload.u64()?;
        if generation_value != 0 {
            return Err(DecodeError::Malformed);
        }
        let generation = Generation::ZERO;
        let contributor =
            ContributorId::try_from_u64(payload.u64()?).map_err(|_| DecodeError::Malformed)?;
        let target_kind = payload.take(1)?[0];
        let path = PathId::try_from_u64(payload.u64()?).map_err(|_| DecodeError::Malformed)?;
        let target = match target_kind {
            0 => EventTarget::File(path),
            1 => EventTarget::Directory(path),
            _ => return Err(DecodeError::Malformed),
        };
        let action = Action::try_from(payload.take(1)?[0]).map_err(|_| DecodeError::Malformed)?;
        let color_flag = payload.take(1)?[0];
        let color_bytes = payload.take(3)?;
        let color = match color_flag {
            0 if color_bytes == [0, 0, 0] => None,
            1 => Some(Rgb8::new(color_bytes[0], color_bytes[1], color_bytes[2])),
            _ => return Err(DecodeError::Malformed),
        };
        events.push(Event::new(
            EventKey::new(timestamp, source_sequence),
            generation,
            contributor,
            target,
            action,
            color,
        ));
    }
    if payload.remaining() != 0 {
        return Err(DecodeError::Malformed);
    }
    match source_sequence_bitmap(&events) {
        Ok(_) => {}
        Err(SequenceError::Allocation) => return Err(DecodeError::Allocation),
        Err(SequenceError::Malformed) => return Err(DecodeError::Malformed),
    }
    let exact_memory =
        indexed_history_memory_bound(&catalog, event_count).ok_or(DecodeError::Malformed)?;
    if exact_memory > key.limits.working_memory_bytes {
        return Err(DecodeError::Malformed);
    }

    for event in &events {
        if catalog.contributor(event.contributor).is_none() {
            return Err(DecodeError::Malformed);
        }
        let path = catalog
            .path(event.target.path_id())
            .ok_or(DecodeError::Malformed)?;
        if path.is_directory() != event.target.is_directory() {
            return Err(DecodeError::Malformed);
        }
    }
    let history = History::new(catalog, events).map_err(|_| DecodeError::Malformed)?;
    let (catalog, events) = history.into_parts();
    let input_identity = InputIdentity {
        digest: input_digest,
        bytes: input_bytes,
    };
    let dataset_identity = DatasetIdentity(dataset_digest);
    let expected_dataset = super::dataset_identity(&catalog, &events, input_identity);
    if expected_dataset != dataset_identity {
        return Err(DecodeError::Malformed);
    }
    Ok(IndexedHistory {
        catalog,
        events,
        input_identity,
        dataset_identity,
        input_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn options() -> IngestOptions {
        IngestOptions::default()
            .with_limits(IngestOptions::default().limits.with_max_record_bytes(256))
    }

    fn history(bytes: &[u8]) -> IndexedHistory {
        super::super::parse_bytes(bytes, options()).expect("test history")
    }

    fn cache(root: &TempDir, max_bytes: u64, max_entry_bytes: u64) -> EventCache {
        EventCache::open(
            CacheConfig::new(root.path().join("cache"))
                .with_max_bytes(max_bytes)
                .with_max_entry_bytes(max_entry_bytes),
        )
        .expect("cache open")
    }

    #[cfg(unix)]
    #[test]
    fn created_directory_is_private_and_existing_insecure_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().expect("tempdir");
        let path = root.path().join("cache");
        let cache = cache(&root, 1 << 20, 1 << 20);
        let mode = fs::metadata(&path)
            .expect("cache metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        drop(cache);

        let mut permissions = fs::metadata(&path).expect("cache metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("insecure permissions");
        assert!(EventCache::open(CacheConfig::new(&path)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cache_directory_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().expect("tempdir");
        let target = root.path().join("target");
        let path = root.path().join("cache");
        fs::create_dir(&target).expect("target directory");
        symlink(&target, &path).expect("cache symlink");
        assert!(EventCache::open(CacheConfig::new(&path)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn held_directory_capability_survives_a_b_a_path_swap() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().expect("tempdir");
        let path = root.path().join("cache");
        let moved = root.path().join("cache-a");
        let cache = cache(&root, 1 << 20, 1 << 20);
        let value = history(b"0|alice|A|src/a.rs\n");
        let key = CacheKey::from_options(value.input_identity(), &options());
        cache.store(&key, &value).expect("store");

        fs::rename(&path, &moved).expect("move private directory");
        fs::create_dir(&path).expect("unvalidated directory");
        let mut permissions = fs::metadata(&path)
            .expect("directory metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("insecure permissions");
        assert!(EventCache::open(CacheConfig::new(&path)).is_err());
        fs::remove_dir(&path).expect("remove unvalidated directory");
        fs::rename(&moved, &path).expect("restore private directory");

        assert_eq!(cache.load(&key).expect("held cache hit"), Some(value));
    }

    #[test]
    fn replacement_quota_counts_old_target_before_staging() {
        let directory = TempDir::new().expect("tempdir");
        let cache_directory = directory.path().join("cache");
        let value = history(b"0|alice|A|src/a-long-name.rs\n");
        let options = options();
        let key = CacheKey::from_options(value.input_identity(), &options);
        let roomy = cache(&directory, 1 << 20, 1 << 20);
        roomy.store(&key, &value).expect("initial store");
        let old_size = fs::metadata(roomy.entry_path(&key))
            .expect("entry metadata")
            .len();
        drop(roomy);

        let constrained = cache(&directory, old_size + 1, old_size + 1);
        assert!(constrained.store(&key, &value).is_err());
        assert_eq!(constrained.load(&key).expect("prior entry"), Some(value));
        assert!(
            fs::read_dir(&cache_directory)
                .expect("cache entries")
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().ends_with(TEMP_SUFFIX))
        );
    }

    #[test]
    fn hit_and_miss_round_trip() {
        let directory = TempDir::new().expect("tempdir");
        let cache = cache(&directory, 1 << 20, 1 << 20);
        let options = options();
        let value = history(b"0|alice|A|src/a.rs\n1|alice|M|src/a.rs\n");
        let key = CacheKey::from_options(value.input_identity(), &options);
        assert!(cache.load(&key).expect("miss").is_none());
        cache.store(&key, &value).expect("store");
        assert_eq!(cache.load(&key).expect("hit"), Some(value));
    }

    #[test]
    fn corruption_and_truncation_are_removed_misses() {
        let directory = TempDir::new().expect("tempdir");
        let cache = cache(&directory, 1 << 20, 1 << 20);
        let options = options();
        let value = history(b"0|alice|A|src/a.rs\n");
        let key = CacheKey::from_options(value.input_identity(), &options);
        cache.store(&key, &value).expect("store");
        let path = cache.entry_path(&key);
        let mut bytes = fs::read(&path).expect("entry");
        let last = bytes.len() - 1;
        bytes[last] ^= 0x80;
        fs::write(&path, &bytes).expect("corrupt");
        assert!(cache.load(&key).expect("corrupt miss").is_none());
        cache.store(&key, &value).expect("restore");
        let bytes = fs::read(&path).expect("entry");
        fs::write(&path, &bytes[..bytes.len() - 1]).expect("truncate");
        assert!(cache.load(&key).expect("truncated miss").is_none());
        assert!(!path.exists());
    }

    #[test]
    fn oversized_entry_is_removed_before_allocation() {
        let directory = TempDir::new().expect("tempdir");
        let cache = cache(&directory, 256, 256);
        let value = history(b"0|alice|A|src/a.rs\n");
        let key = CacheKey::from_options(value.input_identity(), &options());
        let path = cache.entry_path(&key);
        fs::write(&path, vec![0_u8; 257]).expect("oversized file");
        assert!(cache.load(&key).expect("oversized miss").is_none());
        assert!(!path.exists());
    }

    #[test]
    fn failed_store_preserves_prior_valid_entry() {
        let directory = TempDir::new().expect("tempdir");
        let value = history(b"0|alice|A|src/a-long-name.rs\n");
        let key = CacheKey::from_options(value.input_identity(), &options());
        let mut roomy = CacheConfig::new(directory.path().join("cache"))
            .with_max_bytes(1 << 20)
            .with_max_entry_bytes(1 << 20);
        let roomy_cache = EventCache::open(roomy.clone()).expect("roomy cache");
        roomy_cache.store(&key, &value).expect("initial store");
        roomy.max_entry_bytes = 64;
        let too_small = EventCache::open(roomy).expect("small cache");
        assert!(too_small.store(&key, &value).is_err());
        assert_eq!(roomy_cache.load(&key).expect("prior hit"), Some(value));
    }

    #[test]
    fn deterministic_oldest_eviction_obeys_total_cap() {
        let directory = TempDir::new().expect("tempdir");
        let first = history(b"0|alice|A|src/first.rs\n");
        let second = history(b"0|bob|A|src/second.rs\n");
        let probe = cache(&directory, 1 << 20, 1 << 20);
        let key1 = CacheKey::from_options(first.input_identity(), &options());
        probe.store(&key1, &first).expect("first store");
        let size = fs::metadata(probe.entry_path(&key1)).expect("size").len();
        drop(probe);
        let cache = cache(&directory, size + 1, size + 1);
        let key2 = CacheKey::from_options(second.input_identity(), &options());
        cache.store(&key2, &second).expect("second store");
        assert!(cache.load(&key1).expect("oldest miss").is_none());
        assert_eq!(cache.load(&key2).expect("newest hit"), Some(second));
    }

    #[test]
    fn eviction_scan_bounds_adversarial_zero_byte_entries() {
        let root = TempDir::new().expect("tempdir");
        let directory = root.path().join("cache");
        let cache = EventCache::open(
            CacheConfig::new(&directory)
                .with_max_bytes(1 << 20)
                .with_max_entry_bytes(1 << 20)
                .with_max_directory_entries(8)
                .with_max_metadata_bytes(4096),
        )
        .expect("cache open");
        for index in 0..32 {
            fs::write(
                directory.join(format!("adversary-{index}{ENTRY_SUFFIX}")),
                [],
            )
            .expect("zero-byte entry");
        }
        let value = history(b"0|alice|A|src/a.rs\n");
        let key = CacheKey::from_options(value.input_identity(), &options());
        let error = cache.store(&key, &value).expect_err("entry cap");
        assert!(
            matches!(error, CacheError::DirectoryEntryLimit { limit: 8 }),
            "unexpected eviction error: {error:?}"
        );
    }

    #[test]
    fn valid_cache_works_at_directory_entry_limit() {
        let root = TempDir::new().expect("tempdir");
        let directory = root.path().join("cache");
        let cache = EventCache::open(
            CacheConfig::new(&directory)
                .with_max_bytes(1 << 20)
                .with_max_entry_bytes(1 << 20)
                .with_max_directory_entries(2)
                .with_max_metadata_bytes(4096),
        )
        .expect("cache open");
        let value = history(b"0|alice|A|src/a.rs\n");
        let key = CacheKey::from_options(value.input_identity(), &options());
        cache.store(&key, &value).expect("store");
        assert_eq!(cache.load(&key).expect("hit"), Some(value));
    }

    #[cfg(windows)]
    #[test]
    fn owner_rights_sid_encoding_is_stable() {
        assert_eq!(
            WINDOWS_OWNER_RIGHTS_SID.0,
            [1, 1, 0, 0, 0, 0, 0, 3, 4, 0, 0, 0],
        );
    }

    #[test]
    fn concurrent_writers_publish_complete_entries() {
        let directory = TempDir::new().expect("tempdir");
        let cache = Arc::new(cache(&directory, 1 << 20, 1 << 20));
        let value = Arc::new(history(b"0|alice|A|src/a.rs\n1|alice|M|src/a.rs\n"));
        let key = Arc::new(CacheKey::from_options(value.input_identity(), &options()));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            let value = Arc::clone(&value);
            let key = Arc::clone(&key);
            workers.push(thread::spawn(move || {
                cache.store(&key, &value).expect("concurrent store");
            }));
        }
        for worker in workers {
            worker.join().expect("writer join");
        }
        assert_eq!(
            cache.load(&key).expect("concurrent hit"),
            Some((*value).clone())
        );
    }
}
