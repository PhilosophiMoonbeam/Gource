// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Presentation-independent domain contracts for the Rust successor.
//!
//! The core crate intentionally has no window, UI, graphics, filesystem, or
//! asynchronous-runtime dependency.  It owns immutable input identities,
//! canonical events, the repository hierarchy, and deterministic replay math.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;
use thiserror::Error;

/// Current event wire/schema version.
pub const EVENT_SCHEMA_VERSION: u16 = 1;
/// Current catalog wire/schema version.
pub const CATALOG_SCHEMA_VERSION: u16 = 1;
/// Current history wire/schema version.
pub const HISTORY_SCHEMA_VERSION: u16 = 1;
/// Canonical simulation frequency.  It is deliberately not configurable.
pub const SIMULATION_HZ: u64 = 120;

const DEFAULT_PATH_BYTES: usize = 64 * 1024;
const DEFAULT_CONTRIBUTOR_BYTES: usize = 4 * 1024;
const DEFAULT_PATH_COMPONENTS: usize = 256;
const DEFAULT_RECORD_BYTES: u64 = 1024 * 1024;
const DEFAULT_INPUT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const DEFAULT_WORKING_MEMORY_BYTES: u64 = 128 * 1024 * 1024;
const DEFAULT_WORKING_DISK_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// Error returned when a checked identity cannot be represented.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("identity value is out of range: {value}")]
pub struct IdError {
    /// Rejected value.
    pub value: u64,
}

macro_rules! nonzero_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(NonZeroU32);

        impl $name {
            /// Construct an ID.  Zero is reserved as an invalid/sentinel value.
            pub const fn new(value: u32) -> Option<Self> {
                match NonZeroU32::new(value) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }

            /// Construct an ID after checking a wider integer value.
            pub const fn try_from_u64(value: u64) -> Result<Self, IdError> {
                if value == 0 || value > u32::MAX as u64 {
                    Err(IdError { value })
                } else {
                    // The range check above makes this conversion lossless.
                    match NonZeroU32::new(value as u32) {
                        Some(value) => Ok(Self(value)),
                        None => Err(IdError {
                            value: value as u64,
                        }),
                    }
                }
            }

            /// Construct from a non-zero raw integer.
            pub const fn from_raw(value: u32) -> Option<Self> {
                Self::new(value)
            }

            /// Return the stable integer representation.
            pub const fn get(self) -> u32 {
                self.0.get()
            }

            /// Return the stable integer representation as a wider value.
            pub const fn as_u64(self) -> u64 {
                self.get() as u64
            }
        }

        impl TryFrom<u64> for $name {
            type Error = IdError;

            fn try_from(value: u64) -> Result<Self, Self::Error> {
                Self::try_from_u64(value)
            }
        }

        impl From<$name> for u32 {
            fn from(value: $name) -> Self {
                value.get()
            }
        }
    };
}

macro_rules! scalar_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Clone,
            Copy,
            Debug,
            Default,
            Eq,
            Hash,
            Ord,
            PartialEq,
            PartialOrd,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            /// Construct a checked scalar identity.  All `u64` values are
            /// representable; arithmetic is exposed through checked methods.
            pub const fn new(value: u64) -> Option<Self> {
                Some(Self(value))
            }

            /// Construct from a raw value, retaining the checked-ID API shape.
            pub const fn from_raw(value: u64) -> Option<Self> {
                Self::new(value)
            }

            /// Return the stable integer representation.
            pub const fn get(self) -> u64 {
                self.0
            }

            /// Alias for [`Self::get`].
            pub const fn as_u64(self) -> u64 {
                self.get()
            }

            /// Return the next value without wrapping.
            pub const fn checked_add(self, amount: u64) -> Option<Self> {
                match self.0.checked_add(amount) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(value)
            }
        }

        impl From<$name> for u64 {
            fn from(value: $name) -> Self {
                value.get()
            }
        }
    };
}

nonzero_id!(PathId, "Stable interned lexical repository-path identity.");
nonzero_id!(ContributorId, "Stable interned contributor identity.");
nonzero_id!(FileId, "Stable file-incarnation identity.");
nonzero_id!(DirId, "Stable hierarchy-directory identity.");
scalar_id!(
    SourceSeq,
    "Physical source-record sequence, assigned before sorting."
);
scalar_id!(EventIndex, "Zero-based canonical event index.");
scalar_id!(Tick, "Canonical 120-Hz simulation tick.");
scalar_id!(
    Generation,
    "Replay generation used by stale-transition guards."
);

impl EventIndex {
    /// The first canonical event index.
    pub const ZERO: Self = Self(0);
}

impl Tick {
    /// The origin simulation tick.
    pub const ZERO: Self = Self(0);
}

impl Generation {
    /// The initial replay generation.
    pub const ZERO: Self = Self(0);

    /// Advance a generation without wrapping.
    pub const fn next(self) -> Option<Self> {
        self.checked_add(1)
    }
}

/// A strict lexical repository path.  The canonical form has no leading slash;
/// an explicit directory keeps one trailing slash.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct RepositoryPath {
    canonical: String,
    components: Vec<String>,
    directory: bool,
}

/// Short alias used by callers that prefer a path-oriented name.
pub type RepoPath = RepositoryPath;
/// Short alias retained for APIs that call repository paths simply `Path`.
pub type Path = RepositoryPath;

/// Strict lexical path validation failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PathError {
    #[error("path is empty")]
    Empty,
    #[error("path exceeds {limit} bytes")]
    TooLong { limit: usize },
    #[error("path contains forbidden byte 0x{byte:02x}")]
    ForbiddenByte { byte: u8 },
    #[error("path contains an empty component")]
    EmptyComponent,
    #[error("path contains traversal component {component:?}")]
    Traversal { component: String },
    #[error("path has more than {limit} components")]
    TooDeep { limit: usize },
    #[error("path is not valid UTF-8")]
    InvalidUtf8,
}

impl RepositoryPath {
    /// Parse a path using the contract's default finite bounds.
    pub fn parse(value: &str) -> Result<Self, PathError> {
        Self::parse_with_limits(value, DEFAULT_PATH_BYTES, DEFAULT_PATH_COMPONENTS)
    }

    /// Parse UTF-8 bytes without replacement or lossy display conversion.
    pub fn from_bytes(value: &[u8]) -> Result<Self, PathError> {
        let value = std::str::from_utf8(value).map_err(|_| PathError::InvalidUtf8)?;
        Self::parse(value)
    }

    /// Parse a path with caller-selected finite byte/component bounds.
    pub fn parse_with_limits(
        value: &str,
        max_bytes: usize,
        max_components: usize,
    ) -> Result<Self, PathError> {
        if value.is_empty() {
            return Err(PathError::Empty);
        }
        if value.len() > max_bytes {
            return Err(PathError::TooLong { limit: max_bytes });
        }

        for byte in value.bytes() {
            match byte {
                b'\0' | b'|' | b'\n' | b'\r' => {
                    return Err(PathError::ForbiddenByte { byte });
                }
                _ => {}
            }
        }

        // Exactly one leading slash is the legacy virtual-root notation.  It
        // is removed before identity interning; a second slash then becomes an
        // interior empty component and is rejected below.
        let mut body = value;
        if let Some(stripped) = body.strip_prefix('/') {
            body = stripped;
        }
        if body.is_empty() {
            return Err(PathError::Empty);
        }

        let directory = body.ends_with('/');
        if directory {
            body = &body[..body.len() - 1];
            if body.is_empty() {
                return Err(PathError::Empty);
            }
        }

        let mut components = Vec::new();
        for component in body.split('/') {
            if component.is_empty() {
                return Err(PathError::EmptyComponent);
            }
            if component == "." || component == ".." {
                return Err(PathError::Traversal {
                    component: component.to_owned(),
                });
            }
            if components.len() >= max_components {
                return Err(PathError::TooDeep {
                    limit: max_components,
                });
            }
            components.push(component.to_owned());
        }
        if components.is_empty() {
            return Err(PathError::Empty);
        }

        let mut canonical = components.join("/");
        if directory {
            canonical.push('/');
        }
        Ok(Self {
            canonical,
            components,
            directory,
        })
    }

    /// Return canonical lexical spelling (without a virtual-root slash).
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// Alias for [`Self::canonical`].
    pub fn as_str(&self) -> &str {
        self.canonical()
    }

    /// Return canonical bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.canonical.as_bytes()
    }

    /// Return components in lexical order from the virtual root.
    pub fn components(&self) -> &[String] {
        &self.components
    }

    /// Return whether this is an explicit directory target.
    pub const fn is_directory(&self) -> bool {
        self.directory
    }

    /// Return whether this is a file target.
    pub const fn is_file(&self) -> bool {
        !self.directory
    }

    /// Return the number of components.
    pub fn component_count(&self) -> usize {
        self.components.len()
    }

    /// Return the final component.
    pub fn file_name(&self) -> &str {
        // Parsing guarantees at least one component.
        self.components.last().map(String::as_str).unwrap_or("")
    }

    /// Return the lexical component prefix without a trailing target marker.
    pub fn component_prefix(&self, count: usize) -> Option<Vec<String>> {
        (count <= self.components.len()).then(|| self.components[..count].to_vec())
    }

    /// Return the same lexical components as a file target.
    pub fn as_file(&self) -> Self {
        let canonical = self.components.join("/");
        Self {
            canonical,
            components: self.components.clone(),
            directory: false,
        }
    }

    /// Return the same lexical components as a directory target.
    pub fn as_directory(&self) -> Self {
        let mut canonical = self.components.join("/");
        canonical.push('/');
        Self {
            canonical,
            components: self.components.clone(),
            directory: true,
        }
    }
}

impl fmt::Display for RepositoryPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.canonical())
    }
}

/// Limits applied before bounded vectors/strings are allowed to grow.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CatalogLimits {
    pub path_bytes: usize,
    pub contributor_bytes: usize,
    pub path_components: usize,
    pub max_paths: usize,
    pub max_contributors: usize,
}

impl Default for CatalogLimits {
    fn default() -> Self {
        Self {
            path_bytes: DEFAULT_PATH_BYTES,
            contributor_bytes: DEFAULT_CONTRIBUTOR_BYTES,
            path_components: DEFAULT_PATH_COMPONENTS,
            max_paths: usize::MAX,
            max_contributors: usize::MAX,
        }
    }
}

impl CatalogLimits {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.path_bytes == 0 {
            return Err(ConfigError::InvalidLimit("path_bytes"));
        }
        if self.contributor_bytes == 0 {
            return Err(ConfigError::InvalidLimit("contributor_bytes"));
        }
        if self.path_components == 0 {
            return Err(ConfigError::InvalidLimit("path_components"));
        }
        if self.max_paths == 0 {
            return Err(ConfigError::InvalidLimit("max_paths"));
        }
        if self.max_contributors == 0 {
            return Err(ConfigError::InvalidLimit("max_contributors"));
        }
        Ok(())
    }
}

/// Error shared by catalog/history/world construction.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum CoreError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error("contributor name exceeds {limit} bytes")]
    ContributorTooLong { limit: usize },
    #[error("catalog limit exceeded for {resource}: {limit}")]
    CatalogLimit {
        resource: &'static str,
        limit: usize,
    },
    #[error("catalog has no path for {0:?}")]
    UnknownPath(PathId),
    #[error("catalog has no contributor for {0:?}")]
    UnknownContributor(ContributorId),
    #[error("canonical event order violated at index {index}")]
    NonCanonicalEventOrder { index: usize },
    #[error("event target kind does not match its catalog path")]
    TargetKindMismatch,
    #[error("hierarchy ID space exhausted")]
    IdSpaceExhausted,
}

/// Versioned path/contributor intern table shared by ingestion, replay, and
/// rendering.  IDs are allocated in first-interned order and never reused.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    pub version: u16,
    paths: Vec<RepositoryPath>,
    contributors: Vec<String>,
    #[serde(skip)]
    path_index: BTreeMap<String, PathId>,
    #[serde(skip)]
    contributor_index: BTreeMap<String, ContributorId>,
    pub limits: CatalogLimits,
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

impl Catalog {
    /// Create an empty version-1 catalog.
    pub fn new() -> Self {
        Self::with_limits(CatalogLimits::default())
    }

    /// Create an empty catalog with explicit finite limits.
    pub fn with_limits(limits: CatalogLimits) -> Self {
        // Index zero is intentionally unused because identity IDs are
        // non-zero.  This also makes accidental zero IDs fail closed.
        Self {
            version: CATALOG_SCHEMA_VERSION,
            paths: vec![RepositoryPath {
                canonical: String::new(),
                components: Vec::new(),
                directory: false,
            }],
            contributors: vec![String::new()],
            path_index: BTreeMap::new(),
            contributor_index: BTreeMap::new(),
            limits,
        }
    }

    /// Rebuild skipped lookup maps after deserializing a catalog.
    pub fn rebuild_indexes(&mut self) -> Result<(), CoreError> {
        self.path_index.clear();
        self.contributor_index.clear();
        for (index, path) in self.paths.iter().enumerate().skip(1) {
            let id = PathId::try_from_u64(index as u64).map_err(|_| CoreError::IdSpaceExhausted)?;
            if self.path_index.insert(path.canonical.clone(), id).is_some() {
                return Err(CoreError::Path(PathError::Empty));
            }
        }
        for (index, contributor) in self.contributors.iter().enumerate().skip(1) {
            let id = ContributorId::try_from_u64(index as u64)
                .map_err(|_| CoreError::IdSpaceExhausted)?;
            if self
                .contributor_index
                .insert(contributor.clone(), id)
                .is_some()
            {
                return Err(CoreError::UnknownContributor(id));
            }
        }
        Ok(())
    }

    /// Intern an already validated path.
    pub fn intern_path(&mut self, path: &RepositoryPath) -> Result<PathId, CoreError> {
        if let Some(id) = self.path_index.get(path.canonical()).copied() {
            return Ok(id);
        }
        if path.canonical().len() > self.limits.path_bytes {
            return Err(CoreError::Path(PathError::TooLong {
                limit: self.limits.path_bytes,
            }));
        }
        if path.components().len() > self.limits.path_components {
            return Err(CoreError::Path(PathError::TooDeep {
                limit: self.limits.path_components,
            }));
        }
        if self.paths.len() > self.limits.max_paths {
            return Err(CoreError::CatalogLimit {
                resource: "paths",
                limit: self.limits.max_paths,
            });
        }
        let id = PathId::try_from_u64(self.paths.len() as u64)
            .map_err(|_| CoreError::IdSpaceExhausted)?;
        self.paths.push(path.clone());
        self.path_index.insert(path.canonical().to_owned(), id);
        Ok(id)
    }

    /// Parse and intern a path in one operation.
    pub fn intern_path_str(&mut self, path: &str) -> Result<PathId, CoreError> {
        let path = RepositoryPath::parse_with_limits(
            path,
            self.limits.path_bytes,
            self.limits.path_components,
        )?;
        self.intern_path(&path)
    }

    /// Intern an exact contributor key.  Contributor identity is not trimmed,
    /// normalized, or case-folded.
    pub fn intern_contributor(&mut self, contributor: &str) -> Result<ContributorId, CoreError> {
        if let Some(id) = self.contributor_index.get(contributor).copied() {
            return Ok(id);
        }
        if contributor.len() > self.limits.contributor_bytes {
            return Err(CoreError::ContributorTooLong {
                limit: self.limits.contributor_bytes,
            });
        }
        if self.contributors.len() > self.limits.max_contributors {
            return Err(CoreError::CatalogLimit {
                resource: "contributors",
                limit: self.limits.max_contributors,
            });
        }
        let id = ContributorId::try_from_u64(self.contributors.len() as u64)
            .map_err(|_| CoreError::IdSpaceExhausted)?;
        self.contributors.push(contributor.to_owned());
        self.contributor_index.insert(contributor.to_owned(), id);
        Ok(id)
    }

    /// Look up a path by stable ID.
    pub fn path(&self, id: PathId) -> Option<&RepositoryPath> {
        self.paths.get(id.get() as usize)
    }

    /// Look up a contributor by stable ID.
    pub fn contributor(&self, id: ContributorId) -> Option<&str> {
        self.contributors.get(id.get() as usize).map(String::as_str)
    }

    /// Return interned paths in stable ID order.
    pub fn paths(&self) -> &[RepositoryPath] {
        &self.paths[1..]
    }

    /// Return interned contributors in stable ID order.
    pub fn contributors(&self) -> &[String] {
        &self.contributors[1..]
    }

    pub fn path_count(&self) -> usize {
        self.paths.len() - 1
    }

    pub fn contributor_count(&self) -> usize {
        self.contributors.len() - 1
    }

    pub fn version(&self) -> u16 {
        self.version
    }

    pub fn builder() -> CatalogBuilder {
        CatalogBuilder::default()
    }

    /// Build a catalog from already validated parts.
    pub fn from_parts(
        paths: Vec<RepositoryPath>,
        contributors: Vec<String>,
    ) -> Result<Self, CoreError> {
        let mut catalog = Self::new();
        for path in paths {
            catalog.intern_path(&path)?;
        }
        for contributor in contributors {
            catalog.intern_contributor(&contributor)?;
        }
        Ok(catalog)
    }
}

/// Incremental catalog builder convenient for source adapters.
#[derive(Clone, Debug, Default)]
pub struct CatalogBuilder {
    catalog: Catalog,
}

impl CatalogBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limits(limits: CatalogLimits) -> Self {
        Self {
            catalog: Catalog::with_limits(limits),
        }
    }

    pub fn intern_path(&mut self, path: &RepositoryPath) -> Result<PathId, CoreError> {
        self.catalog.intern_path(path)
    }

    pub fn intern_path_str(&mut self, path: &str) -> Result<PathId, CoreError> {
        self.catalog.intern_path_str(path)
    }

    pub fn intern_contributor(&mut self, contributor: &str) -> Result<ContributorId, CoreError> {
        self.catalog.intern_contributor(contributor)
    }

    pub fn finish(self) -> Catalog {
        self.catalog
    }

    pub fn build(self) -> Catalog {
        self.finish()
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }
}

/// Canonical order key: timestamp first, physical source sequence second.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct EventKey {
    pub timestamp: i64,
    pub source_sequence: SourceSeq,
}

impl EventKey {
    pub const fn new(timestamp: i64, source_sequence: SourceSeq) -> Self {
        Self {
            timestamp,
            source_sequence,
        }
    }

    pub const fn timestamp(self) -> i64 {
        self.timestamp
    }

    pub const fn source_sequence(self) -> SourceSeq {
        self.source_sequence
    }
}

/// Strict replay action vocabulary.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum Action {
    Add,
    Modify,
    Delete,
}

impl Action {
    /// Associated compatibility spellings for code that uses wire letters.
    pub const A: Self = Self::Add;
    pub const M: Self = Self::Modify;
    pub const D: Self = Self::Delete;

    pub const fn as_byte(self) -> u8 {
        match self {
            Self::Add => b'A',
            Self::Modify => b'M',
            Self::Delete => b'D',
        }
    }

    pub const fn as_char(self) -> char {
        self.as_byte() as char
    }

    pub const fn is_delete(self) -> bool {
        matches!(self, Self::Delete)
    }
}

impl TryFrom<u8> for Action {
    type Error = ActionError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            b'A' => Ok(Self::Add),
            b'M' => Ok(Self::Modify),
            b'D' => Ok(Self::Delete),
            _ => Err(ActionError { value }),
        }
    }
}

impl TryFrom<char> for Action {
    type Error = ActionError;

    fn try_from(value: char) -> Result<Self, Self::Error> {
        Self::try_from(value as u8).and_then(|action| {
            if value.is_ascii() {
                Ok(action)
            } else {
                Err(ActionError { value: value as u8 })
            }
        })
    }
}

impl TryFrom<&str> for Action {
    type Error = ActionError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let bytes = value.as_bytes();
        if bytes.len() != 1 {
            return Err(ActionError {
                value: bytes.first().copied().unwrap_or(0),
            });
        }
        Self::try_from(bytes[0])
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Add => "A",
            Self::Modify => "M",
            Self::Delete => "D",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("unsupported action byte 0x{value:02x}")]
pub struct ActionError {
    pub value: u8,
}

/// Target kind is explicit so a trailing slash cannot be lost in a catalog.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum EventTarget {
    File(PathId),
    Directory(PathId),
}

impl EventTarget {
    pub const fn file(path: PathId) -> Self {
        Self::File(path)
    }

    pub const fn directory(path: PathId) -> Self {
        Self::Directory(path)
    }

    pub const fn path_id(self) -> PathId {
        match self {
            Self::File(id) | Self::Directory(id) => id,
        }
    }

    pub const fn is_directory(self) -> bool {
        matches!(self, Self::Directory(_))
    }

    pub const fn is_file(self) -> bool {
        matches!(self, Self::File(_))
    }
}

/// Compact gamma-space colour value carried by source events and snapshots.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
pub struct Rgb8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb8 {
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0 };
    pub const WHITE: Self = Self {
        r: 255,
        g: 255,
        b: 255,
    };

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub const fn as_array(self) -> [u8; 3] {
        [self.r, self.g, self.b]
    }

    pub fn from_hex(value: &str) -> Result<Self, ColorError> {
        let value = value.strip_prefix('#').unwrap_or(value);
        if value.len() != 6 || !value.is_ascii() {
            return Err(ColorError::InvalidLength);
        }
        let bytes = value.as_bytes();
        let parse =
            |hi: u8, lo: u8| -> Option<u8> { Some((hex_nibble(hi)? << 4) | hex_nibble(lo)?) };
        Ok(Self {
            r: parse(bytes[0], bytes[1]).ok_or(ColorError::InvalidDigit)?,
            g: parse(bytes[2], bytes[3]).ok_or(ColorError::InvalidDigit)?,
            b: parse(bytes[4], bytes[5]).ok_or(ColorError::InvalidDigit)?,
        })
    }

    pub fn to_hex(self) -> String {
        format!("{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ColorError {
    #[error("colour must contain exactly six hexadecimal digits")]
    InvalidLength,
    #[error("colour contains a non-hexadecimal digit")]
    InvalidDigit,
}

/// A normalized canonical replay event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub version: u16,
    pub key: EventKey,
    pub generation: Generation,
    pub contributor: ContributorId,
    pub action: Action,
    pub target: EventTarget,
    pub color: Option<Rgb8>,
}

impl Event {
    pub fn new(
        key: EventKey,
        generation: Generation,
        contributor: ContributorId,
        target: EventTarget,
        action: Action,
        color: Option<Rgb8>,
    ) -> Self {
        Self {
            version: EVENT_SCHEMA_VERSION,
            key,
            generation,
            contributor,
            action,
            target,
            color,
        }
    }

    pub fn new_v1(
        key: EventKey,
        generation: Generation,
        contributor: ContributorId,
        target: EventTarget,
        action: Action,
        color: Option<Rgb8>,
    ) -> Self {
        Self::new(key, generation, contributor, target, action, color)
    }

    pub const fn timestamp(&self) -> i64 {
        self.key.timestamp
    }

    pub const fn source_sequence(&self) -> SourceSeq {
        self.key.source_sequence
    }

    pub const fn path_id(&self) -> PathId {
        self.target.path_id()
    }

    pub const fn is_directory(&self) -> bool {
        self.target.is_directory()
    }
}

/// Immutable finite canonical event history.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct History {
    pub version: u16,
    catalog: Catalog,
    events: Vec<Event>,
}

pub type EventHistory = History;

impl History {
    /// Construct a history, requiring canonical `(timestamp, source_sequence)`
    /// order and preserving duplicates.
    pub fn new(catalog: Catalog, events: Vec<Event>) -> Result<Self, CoreError> {
        for index in 1..events.len() {
            if events[index - 1].key > events[index].key {
                return Err(CoreError::NonCanonicalEventOrder { index });
            }
        }
        Ok(Self {
            version: HISTORY_SCHEMA_VERSION,
            catalog,
            events,
        })
    }

    pub fn from_events(catalog: Catalog, events: Vec<Event>) -> Result<Self, CoreError> {
        Self::new(catalog, events)
    }

    pub fn empty() -> Self {
        Self {
            version: HISTORY_SCHEMA_VERSION,
            catalog: Catalog::new(),
            events: Vec::new(),
        }
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn into_parts(self) -> (Catalog, Vec<Event>) {
        (self.catalog, self.events)
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// Read-only source boundary consumed by simulation.  Sources are finite and
/// globally canonical before this trait is exposed to consumers.
pub trait HistorySource {
    fn catalog(&self) -> &Catalog;
    fn events(&self) -> &[Event];

    fn event(&self, index: EventIndex) -> Option<&Event> {
        self.events().get(index.get() as usize)
    }

    fn event_count(&self) -> usize {
        self.events().len()
    }

    fn len(&self) -> usize {
        self.event_count()
    }

    fn is_empty(&self) -> bool {
        self.event_count() == 0
    }
}

impl HistorySource for History {
    fn catalog(&self) -> &Catalog {
        self.catalog()
    }

    fn events(&self) -> &[Event] {
        self.events()
    }
}

/// Directory node in the compressed component-radix hierarchy.  `label` is a
/// run of one or more components; branches split only when component prefixes
/// diverge.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DirNode {
    pub id: DirId,
    pub parent: Option<DirId>,
    pub label: Vec<String>,
    pub explicit: bool,
    pub children: BTreeMap<String, DirId>,
    pub files: BTreeMap<String, FileId>,
}

impl DirNode {
    pub fn is_root(&self) -> bool {
        self.parent.is_none()
    }

    pub fn depth(&self) -> usize {
        self.label.len()
    }

    pub fn name(&self) -> &str {
        self.label.last().map(String::as_str).unwrap_or("")
    }
}

/// Current or historical file incarnation.  Dead records remain addressable by
/// ID so stale actions cannot accidentally mutate a recreated file.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileNode {
    pub id: FileId,
    pub path_id: PathId,
    pub parent: DirId,
    pub name: String,
    pub alive: bool,
    pub last_event: EventKey,
    pub contributor: ContributorId,
    pub action: Action,
    pub color: Option<Rgb8>,
}

/// Visible/action activity emitted for every accepted event, including an
/// absent-file delete.  `file_id == None` is intentional and carries activity
/// without creating topology.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub key: EventKey,
    pub generation: Generation,
    pub contributor: ContributorId,
    pub path_id: PathId,
    pub file_id: Option<FileId>,
    pub action: Action,
    pub color: Option<Rgb8>,
}

/// Stable summary of one world transition.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorldDelta {
    pub key: Option<EventKey>,
    pub generation: Generation,
    pub created_files: Vec<FileId>,
    pub modified_files: Vec<FileId>,
    pub deleted_files: Vec<FileId>,
    pub removed_dirs: Vec<DirId>,
    pub activities: Vec<Activity>,
}

impl WorldDelta {
    pub fn is_empty(&self) -> bool {
        self.created_files.is_empty()
            && self.modified_files.is_empty()
            && self.deleted_files.is_empty()
            && self.removed_dirs.is_empty()
            && self.activities.is_empty()
    }

    pub fn event_key(&self) -> Option<EventKey> {
        self.key
    }
}

/// Hierarchy transition failure.  Stale transitions are errors rather than
/// silently mutating a newer file incarnation.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum WorldError {
    #[error("event generation {event:?} is older than world generation {current:?}")]
    StaleGeneration {
        event: Generation,
        current: Generation,
    },
    #[error("event key {event:?} is not newer than world key {current:?}")]
    StaleEventKey { event: EventKey, current: EventKey },
    #[error("event references unknown path {0:?}")]
    UnknownPath(PathId),
    #[error("event target kind does not match catalog path")]
    TargetKindMismatch,
    #[error("add/modify is unsupported for an explicit directory target")]
    DirectoryActionUnsupported,
    #[error("modify requested for absent file {0:?}")]
    MissingFile(PathId),
    #[error("file target conflicts with an existing directory subtree {0:?}")]
    FileDirectoryConflict(PathId),
    #[error("hierarchy ID space exhausted")]
    IdSpaceExhausted,
}

/// Deterministic component-radix world.  All public iteration methods sort by
/// stable IDs; BTree maps make topology mutation independent of hash order.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct World {
    pub generation: Generation,
    root: DirId,
    next_dir: u32,
    next_file: u32,
    dirs: BTreeMap<DirId, DirNode>,
    files: BTreeMap<FileId, FileNode>,
    active_paths: BTreeMap<PathId, FileId>,
    active_components: BTreeMap<Vec<String>, FileId>,
    explicit_dirs: BTreeMap<Vec<String>, DirId>,
    last_key: Option<EventKey>,
    #[serde(skip)]
    catalog: Option<Catalog>,
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl World {
    /// Create a world with only its virtual root.  Callers pass a catalog to
    /// [`Self::apply_event`] so the world remains usable with immutable sources.
    pub fn new() -> Self {
        let root = DirId::new(1).expect("literal root ID is non-zero");
        let mut dirs = BTreeMap::new();
        dirs.insert(
            root,
            DirNode {
                id: root,
                parent: None,
                label: Vec::new(),
                explicit: true,
                children: BTreeMap::new(),
                files: BTreeMap::new(),
            },
        );
        Self {
            generation: Generation::ZERO,
            root,
            next_dir: 2,
            next_file: 1,
            dirs,
            files: BTreeMap::new(),
            active_paths: BTreeMap::new(),
            active_components: BTreeMap::new(),
            explicit_dirs: BTreeMap::new(),
            last_key: None,
            catalog: None,
        }
    }

    /// Create a world retaining a catalog for [`Self::apply_owned`].
    pub fn with_catalog(catalog: Catalog) -> Self {
        let mut world = Self::new();
        world.catalog = Some(catalog);
        world
    }

    pub fn root(&self) -> DirId {
        self.root
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    pub fn last_event_key(&self) -> Option<EventKey> {
        self.last_key
    }

    pub fn directories(&self) -> Vec<&DirNode> {
        self.dirs.values().collect()
    }

    pub fn directory(&self, id: DirId) -> Option<&DirNode> {
        self.dirs.get(&id)
    }

    pub fn files(&self) -> Vec<&FileNode> {
        self.files.values().collect()
    }

    pub fn file(&self, id: FileId) -> Option<&FileNode> {
        self.files.get(&id)
    }

    pub fn active_files(&self) -> Vec<&FileNode> {
        self.files.values().filter(|file| file.alive).collect()
    }

    pub fn active_file(&self, path: PathId) -> Option<FileId> {
        self.active_paths.get(&path).copied()
    }

    pub fn active_file_node(&self, path: PathId) -> Option<&FileNode> {
        self.active_file(path).and_then(|id| self.files.get(&id))
    }

    pub fn active_directory(&self, components: &[String]) -> Option<DirId> {
        self.explicit_dirs.get(components).copied()
    }

    /// Change generation explicitly, invalidating key ordering from the old
    /// replay.  Existing topology is retained for callers restoring a state.
    pub fn set_generation(&mut self, generation: Generation) {
        self.generation = generation;
        self.last_key = None;
    }

    /// Reset all topology while retaining ID allocation monotonicity.
    pub fn reset(&mut self, generation: Generation) {
        let next_dir = self.next_dir;
        let next_file = self.next_file;
        *self = Self::new();
        self.next_dir = next_dir;
        self.next_file = next_file;
        self.generation = generation;
    }

    /// Apply an event using an explicit immutable catalog.
    pub fn apply_event(
        &mut self,
        event: &Event,
        catalog: &Catalog,
    ) -> Result<WorldDelta, WorldError> {
        self.apply_inner(event, catalog)
    }

    /// Alias used by replay implementations.
    pub fn apply_with_catalog(
        &mut self,
        event: &Event,
        catalog: &Catalog,
    ) -> Result<WorldDelta, WorldError> {
        self.apply_event(event, catalog)
    }

    /// Apply an event using the catalog supplied to [`Self::with_catalog`].
    pub fn apply_owned(&mut self, event: &Event) -> Result<WorldDelta, WorldError> {
        let catalog = self
            .catalog
            .clone()
            .ok_or(WorldError::UnknownPath(event.path_id()))?;
        self.apply_inner(event, &catalog)
    }

    fn apply_inner(&mut self, event: &Event, catalog: &Catalog) -> Result<WorldDelta, WorldError> {
        let path = catalog
            .path(event.path_id())
            .ok_or(WorldError::UnknownPath(event.path_id()))?;
        if path.is_directory() != event.is_directory() {
            return Err(WorldError::TargetKindMismatch);
        }
        if event.generation < self.generation {
            return Err(WorldError::StaleGeneration {
                event: event.generation,
                current: self.generation,
            });
        }
        if event.generation > self.generation {
            self.generation = event.generation;
            self.last_key = None;
        }
        if let Some(current) = self.last_key
            && event.key <= current
        {
            return Err(WorldError::StaleEventKey {
                event: event.key,
                current,
            });
        }

        let mut delta = WorldDelta {
            key: Some(event.key),
            generation: event.generation,
            ..WorldDelta::default()
        };
        match event.target {
            EventTarget::File(path_id) => {
                self.apply_file_event(event, path_id, path.components(), &mut delta)?;
            }
            EventTarget::Directory(path_id) => {
                self.apply_directory_event(event, path_id, path.components(), &mut delta)?;
            }
        }
        delta.activities.push(Activity {
            key: event.key,
            generation: event.generation,
            contributor: event.contributor,
            path_id: event.path_id(),
            file_id: delta
                .created_files
                .last()
                .copied()
                .or_else(|| delta.modified_files.last().copied()),
            action: event.action,
            color: event.color,
        });
        self.last_key = Some(event.key);
        Ok(delta)
    }

    fn apply_file_event(
        &mut self,
        event: &Event,
        path_id: PathId,
        components: &[String],
        delta: &mut WorldDelta,
    ) -> Result<(), WorldError> {
        // A file cannot coexist with a current descendant file or an explicit
        // directory at the same lexical prefix.
        let has_descendant = self.active_components.keys().any(|candidate| {
            candidate.len() > components.len() && candidate.starts_with(components)
        });
        if has_descendant || self.explicit_dirs.contains_key(components) {
            return match event.action {
                Action::Delete => self.delete_file(path_id, components, delta),
                Action::Add | Action::Modify => Err(WorldError::FileDirectoryConflict(path_id)),
            };
        }

        match event.action {
            Action::Add => {
                // Adding a descendant forces an old file prefix out of the
                // tree, matching the legacy visible hierarchy without aliasing
                // two file leaves at one component boundary.
                let ancestors: Vec<(Vec<String>, FileId)> = self
                    .active_components
                    .iter()
                    .filter(|(candidate, _)| {
                        candidate.len() < components.len() && components.starts_with(candidate)
                    })
                    .map(|(candidate, id)| (candidate.clone(), *id))
                    .collect();
                for (ancestor, _) in ancestors {
                    self.remove_file_by_components(&ancestor, delta);
                }
                // A duplicate add is a replacement incarnation.  The old ID
                // is dead and is never reused.
                if self.active_components.contains_key(components) {
                    self.remove_file_by_components(components, delta);
                }
                let parent = self.ensure_directory(&components[..components.len() - 1])?;
                let id = self.allocate_file()?;
                let name = components.last().cloned().unwrap_or_default();
                let file = FileNode {
                    id,
                    path_id,
                    parent,
                    name: name.clone(),
                    alive: true,
                    last_event: event.key,
                    contributor: event.contributor,
                    action: event.action,
                    color: event.color,
                };
                self.files.insert(id, file);
                self.active_paths.insert(path_id, id);
                self.active_components.insert(components.to_vec(), id);
                self.dirs
                    .get_mut(&parent)
                    .expect("parent directory exists")
                    .files
                    .insert(name, id);
                delta.created_files.push(id);
            }
            Action::Modify => {
                let Some(id) = self.active_paths.get(&path_id).copied() else {
                    return Err(WorldError::MissingFile(path_id));
                };
                let file = self.files.get_mut(&id).expect("active file exists");
                file.last_event = event.key;
                file.contributor = event.contributor;
                file.action = event.action;
                file.color = event.color;
                delta.modified_files.push(id);
            }
            Action::Delete => self.delete_file(path_id, components, delta)?,
        }
        Ok(())
    }

    fn apply_directory_event(
        &mut self,
        event: &Event,
        path_id: PathId,
        components: &[String],
        delta: &mut WorldDelta,
    ) -> Result<(), WorldError> {
        match event.action {
            Action::Add | Action::Modify => return Err(WorldError::DirectoryActionUnsupported),
            Action::Delete => {
                let descendants: Vec<(Vec<String>, FileId, PathId)> = self
                    .active_components
                    .iter()
                    .filter_map(|(candidate, id)| {
                        if candidate.starts_with(components) {
                            self.files
                                .get(id)
                                .map(|file| (candidate.clone(), *id, file.path_id))
                        } else {
                            None
                        }
                    })
                    .collect();
                // BTreeMap iteration is lexical by component vector, which is
                // the required deterministic subtree expansion order.
                for (candidate, _, path_id) in descendants {
                    self.remove_file_by_components(&candidate, delta);
                    // Preserve the path ID in the deleted list through the
                    // file ID; the incoming directory activity covers the
                    // directory target itself.
                    let _ = path_id;
                }
                if let Some(dir) = self.explicit_dirs.remove(components) {
                    self.prune_from(dir, &mut delta.removed_dirs);
                } else if let Some(dir) = self.find_directory(components) {
                    self.prune_from(dir, &mut delta.removed_dirs);
                }
                // An absent directory delete is intentionally accepted: its
                // activity is still returned by apply_inner.
                let _ = path_id;
            }
        }
        Ok(())
    }

    fn delete_file(
        &mut self,
        path_id: PathId,
        components: &[String],
        delta: &mut WorldDelta,
    ) -> Result<(), WorldError> {
        if self.active_paths.contains_key(&path_id) {
            self.remove_file_by_components(components, delta);
        } else {
            // D(absent) is not a topology error: activity remains visible.
            let _ = components;
        }
        Ok(())
    }

    fn remove_file_by_components(&mut self, components: &[String], delta: &mut WorldDelta) {
        let Some(id) = self.active_components.remove(components) else {
            return;
        };
        self.active_paths.retain(|_, value| *value != id);
        let Some(mut file) = self.files.remove(&id) else {
            return;
        };
        file.alive = false;
        let parent = file.parent;
        if let Some(dir) = self.dirs.get_mut(&parent) {
            dir.files.remove(&file.name);
        }
        self.files.insert(id, file);
        delta.deleted_files.push(id);
        self.prune_from(parent, &mut delta.removed_dirs);
    }

    fn allocate_dir(&mut self) -> Result<DirId, WorldError> {
        let id = DirId::new(self.next_dir).ok_or(WorldError::IdSpaceExhausted)?;
        self.next_dir = self
            .next_dir
            .checked_add(1)
            .ok_or(WorldError::IdSpaceExhausted)?;
        Ok(id)
    }

    fn allocate_file(&mut self) -> Result<FileId, WorldError> {
        let id = FileId::new(self.next_file).ok_or(WorldError::IdSpaceExhausted)?;
        self.next_file = self
            .next_file
            .checked_add(1)
            .ok_or(WorldError::IdSpaceExhausted)?;
        Ok(id)
    }

    fn find_directory(&self, components: &[String]) -> Option<DirId> {
        if components.is_empty() {
            return Some(self.root);
        }
        self.dirs
            .iter()
            .find_map(|(id, _)| (self.dir_components(*id) == components).then_some(*id))
    }

    fn dir_components(&self, id: DirId) -> Vec<String> {
        let Some(dir) = self.dirs.get(&id) else {
            return Vec::new();
        };
        let mut result = dir.label.clone();
        let mut parent = dir.parent;
        while let Some(parent_id) = parent {
            let Some(parent_dir) = self.dirs.get(&parent_id) else {
                break;
            };
            result.splice(0..0, parent_dir.label.clone());
            parent = parent_dir.parent;
        }
        result
    }

    fn ensure_directory(&mut self, components: &[String]) -> Result<DirId, WorldError> {
        if components.is_empty() {
            return Ok(self.root);
        }
        let mut parent = self.root;
        let mut position = 0;
        while position < components.len() {
            let key = components[position].clone();
            let child = self
                .dirs
                .get(&parent)
                .and_then(|dir| dir.children.get(&key).copied());
            let Some(child) = child else {
                let id = self.allocate_dir()?;
                let label = components[position..].to_vec();
                self.dirs.insert(
                    id,
                    DirNode {
                        id,
                        parent: Some(parent),
                        label,
                        explicit: false,
                        children: BTreeMap::new(),
                        files: BTreeMap::new(),
                    },
                );
                self.dirs
                    .get_mut(&parent)
                    .expect("parent directory exists")
                    .children
                    .insert(key, id);
                return Ok(id);
            };
            let label = self
                .dirs
                .get(&child)
                .map(|dir| dir.label.clone())
                .unwrap_or_default();
            let common = label
                .iter()
                .zip(components[position..].iter())
                .take_while(|(left, right)| left == right)
                .count();
            if common == label.len() {
                position += common;
                parent = child;
                continue;
            }

            // Split at the first diverging component.  Existing files and
            // descendants stay attached to the old suffix node.
            let split = self.allocate_dir()?;
            let prefix = label[..common].to_vec();
            let suffix = label[common..].to_vec();
            self.dirs.insert(
                split,
                DirNode {
                    id: split,
                    parent: Some(parent),
                    label: prefix,
                    explicit: false,
                    children: BTreeMap::new(),
                    files: BTreeMap::new(),
                },
            );
            {
                let old = self.dirs.get_mut(&child).expect("child exists");
                old.parent = Some(split);
                old.label = suffix;
            }
            let old_key = self
                .dirs
                .get(&child)
                .and_then(|dir| dir.label.first().cloned())
                .expect("split suffix is non-empty");
            self.dirs
                .get_mut(&split)
                .expect("split exists")
                .children
                .insert(old_key, child);
            self.dirs
                .get_mut(&parent)
                .expect("parent exists")
                .children
                .insert(key, split);
            position += common;
            parent = split;
        }
        Ok(parent)
    }

    fn prune_from(&mut self, mut id: DirId, removed: &mut Vec<DirId>) {
        while id != self.root {
            let removable = self
                .dirs
                .get(&id)
                .map(|dir| !dir.explicit && dir.children.is_empty() && dir.files.is_empty())
                .unwrap_or(false);
            if !removable {
                break;
            }
            let parent = self.dirs.get(&id).and_then(|dir| dir.parent);
            let Some(parent) = parent else {
                break;
            };
            let key = self
                .dirs
                .get(&id)
                .and_then(|dir| dir.label.first().cloned());
            self.dirs.remove(&id);
            if let Some(key) = key
                && let Some(parent_dir) = self.dirs.get_mut(&parent)
            {
                parent_dir.children.remove(&key);
            }
            removed.push(id);
            id = parent;
        }
    }

    /// Return active files in lexical path order rather than allocation order.
    pub fn active_files_lexical<'a>(&'a self, catalog: &'a Catalog) -> Vec<&'a FileNode> {
        let mut files: Vec<&FileNode> = self.active_files();
        files.sort_by(|left, right| {
            catalog
                .path(left.path_id)
                .map(RepositoryPath::canonical)
                .cmp(&catalog.path(right.path_id).map(RepositoryPath::canonical))
                .then_with(|| left.id.cmp(&right.id))
        });
        files
    }
}

/// Finite resource limits from the versioned replay configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    pub record_bytes: u64,
    pub input_bytes: u64,
    pub path_bytes: u64,
    pub contributor_bytes: u64,
    pub path_components: u64,
    pub working_memory_bytes: u64,
    pub working_disk_bytes: u64,
    pub max_events: u64,
    pub max_paths: u64,
    pub max_contributors: u64,
}

pub type ResourceLimits = Limits;

impl Default for Limits {
    fn default() -> Self {
        Self {
            record_bytes: DEFAULT_RECORD_BYTES,
            input_bytes: DEFAULT_INPUT_BYTES,
            path_bytes: DEFAULT_PATH_BYTES as u64,
            contributor_bytes: DEFAULT_CONTRIBUTOR_BYTES as u64,
            path_components: DEFAULT_PATH_COMPONENTS as u64,
            working_memory_bytes: DEFAULT_WORKING_MEMORY_BYTES,
            working_disk_bytes: DEFAULT_WORKING_DISK_BYTES,
            max_events: u64::MAX,
            max_paths: u64::MAX,
            max_contributors: u64::MAX,
        }
    }
}

impl Limits {
    pub fn validate(&self) -> Result<(), ConfigError> {
        let checks = [
            ("record_bytes", self.record_bytes),
            ("input_bytes", self.input_bytes),
            ("path_bytes", self.path_bytes),
            ("contributor_bytes", self.contributor_bytes),
            ("path_components", self.path_components),
            ("working_memory_bytes", self.working_memory_bytes),
            ("working_disk_bytes", self.working_disk_bytes),
            ("max_events", self.max_events),
            ("max_paths", self.max_paths),
            ("max_contributors", self.max_contributors),
        ];
        for (name, value) in checks {
            if value == 0 {
                return Err(ConfigError::InvalidLimit(name));
            }
        }
        Ok(())
    }
}

/// Camera policy included in replay identity.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum CameraMode {
    #[default]
    Overview,
    Track,
}

impl TryFrom<&str> for CameraMode {
    type Error = ConfigError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "overview" => Ok(Self::Overview),
            "track" => Ok(Self::Track),
            _ => Err(ConfigError::InvalidCameraMode(value.to_owned())),
        }
    }
}

/// Pure automatic camera state.  Manual presentation overrides belong outside
/// core and therefore cannot perturb replay/checkpoint determinism.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CameraState {
    pub mode: CameraMode,
    pub center: [f64; 2],
    pub zoom: f64,
    pub rotation: f64,
    pub viewport: [u32; 2],
}

impl Default for CameraState {
    fn default() -> Self {
        Self::new(CameraMode::Overview)
    }
}

impl CameraState {
    pub fn new(mode: CameraMode) -> Self {
        Self {
            mode,
            center: [0.0, 0.0],
            zoom: 1.0,
            rotation: 0.0,
            viewport: [1280, 720],
        }
    }

    pub fn with_viewport(mut self, width: u32, height: u32) -> Self {
        self.viewport = [width, height];
        self
    }

    pub fn set_viewport(&mut self, width: u32, height: u32) {
        self.viewport = [width, height];
    }

    pub fn set_mode(&mut self, mode: CameraMode) {
        self.mode = mode;
    }

    pub fn aspect_ratio(&self) -> f64 {
        if self.viewport[1] == 0 {
            1.0
        } else {
            self.viewport[0] as f64 / self.viewport[1] as f64
        }
    }

    /// Compute an overview camera from immutable bounds `[min_x,min_y,max_x,max_y]`.
    pub fn overview(bounds: [f64; 4], viewport: [u32; 2]) -> Self {
        let width = (bounds[2] - bounds[0]).abs().max(1.0);
        let height = (bounds[3] - bounds[1]).abs().max(1.0);
        let aspect = if viewport[1] == 0 {
            1.0
        } else {
            viewport[0] as f64 / viewport[1] as f64
        };
        let fit_width = width / aspect.max(f64::MIN_POSITIVE);
        let zoom = 1.0 / fit_width.max(height);
        Self {
            mode: CameraMode::Overview,
            center: [(bounds[0] + bounds[2]) * 0.5, (bounds[1] + bounds[3]) * 0.5],
            zoom,
            rotation: 0.0,
            viewport,
        }
    }

    /// Deterministic track update toward a target point.  The operation is
    /// pure with respect to the world; callers choose when to commit the value.
    pub fn tracked(mut self, target: [f64; 2]) -> Self {
        self.mode = CameraMode::Track;
        self.center = target;
        self
    }
}

/// Error for invalid rational values/configuration.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum RationalError {
    #[error("rational denominator must be positive")]
    NonPositiveDenominator,
    #[error("rational value is not finite or is negative")]
    NonFinite,
    #[error("rational arithmetic overflow")]
    Overflow,
}

/// Reduced signed rational used by the canonical playback clock.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Rational {
    pub numerator: i128,
    pub denominator: i128,
}

impl Rational {
    pub const ZERO: Self = Self {
        numerator: 0,
        denominator: 1,
    };

    pub const ONE: Self = Self {
        numerator: 1,
        denominator: 1,
    };

    pub fn new(numerator: i128, denominator: i128) -> Result<Self, RationalError> {
        if denominator <= 0 {
            return Err(RationalError::NonPositiveDenominator);
        }
        if numerator == 0 {
            return Ok(Self::ZERO);
        }
        let divisor = gcd_i128(numerator.unsigned_abs(), denominator as u128) as i128;
        Ok(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }

    pub fn from_u64(value: u64) -> Self {
        Self {
            numerator: value as i128,
            denominator: 1,
        }
    }

    /// Convert a finite non-negative float through its decimal spelling.  The
    /// float is used only at configuration ingress; tick/event ordering uses
    /// the resulting integer ratio thereafter.
    pub fn from_f64(value: f64) -> Result<Self, RationalError> {
        if !value.is_finite() || value < 0.0 {
            return Err(RationalError::NonFinite);
        }
        let spelling = value.to_string();
        parse_decimal_rational(&spelling)
    }

    pub fn checked_add(self, other: Self) -> Result<Self, RationalError> {
        let left = self
            .numerator
            .checked_mul(other.denominator)
            .ok_or(RationalError::Overflow)?;
        let right = other
            .numerator
            .checked_mul(self.denominator)
            .ok_or(RationalError::Overflow)?;
        let numerator = left.checked_add(right).ok_or(RationalError::Overflow)?;
        let denominator = self
            .denominator
            .checked_mul(other.denominator)
            .ok_or(RationalError::Overflow)?;
        Self::new(numerator, denominator)
    }

    pub fn checked_mul(self, other: Self) -> Result<Self, RationalError> {
        let numerator = self
            .numerator
            .checked_mul(other.numerator)
            .ok_or(RationalError::Overflow)?;
        let denominator = self
            .denominator
            .checked_mul(other.denominator)
            .ok_or(RationalError::Overflow)?;
        Self::new(numerator, denominator)
    }

    pub fn checked_mul_u64(self, value: u64) -> Result<Self, RationalError> {
        Self::new(
            self.numerator
                .checked_mul(value as i128)
                .ok_or(RationalError::Overflow)?,
            self.denominator,
        )
    }

    pub fn floor(self) -> i128 {
        self.numerator.div_euclid(self.denominator)
    }

    pub fn ceil(self) -> i128 {
        let quotient = self.numerator.div_euclid(self.denominator);
        if self.numerator.rem_euclid(self.denominator) == 0 {
            quotient
        } else {
            quotient
                .checked_add(1)
                .expect("ceil is representable for a positive-denominator rational")
        }
    }

    pub fn as_f64(self) -> f64 {
        self.numerator as f64 / self.denominator as f64
    }
}

fn gcd_i128(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left.max(1)
}

fn parse_decimal_rational(value: &str) -> Result<Rational, RationalError> {
    let (mantissa, exponent) = match value.find(['e', 'E']) {
        Some(index) => {
            let exponent = value[index + 1..]
                .parse::<i32>()
                .map_err(|_| RationalError::NonFinite)?;
            (&value[..index], exponent)
        }
        None => (value, 0),
    };
    let mut digits = String::new();
    let mut fractional = 0i32;
    for byte in mantissa.bytes() {
        match byte {
            b'.' => {}
            b'0'..=b'9' => {
                digits.push(byte as char);
                if fractional > -1 {
                    fractional += 1;
                }
            }
            _ => return Err(RationalError::NonFinite),
        }
    }
    if let Some(dot) = mantissa.find('.') {
        fractional = (mantissa.len() - dot - 1) as i32;
    } else {
        fractional = 0;
    }
    let numerator = digits
        .parse::<i128>()
        .map_err(|_| RationalError::Overflow)?;
    let scale = exponent
        .checked_sub(fractional)
        .ok_or(RationalError::Overflow)?;
    if scale >= 0 {
        let factor = pow10_i128(scale as u32)?;
        Rational::new(
            numerator
                .checked_mul(factor)
                .ok_or(RationalError::Overflow)?,
            1,
        )
    } else {
        let factor = pow10_i128((-scale) as u32)?;
        Rational::new(numerator, factor)
    }
}

fn pow10_i128(power: u32) -> Result<i128, RationalError> {
    let mut result = 1i128;
    for _ in 0..power {
        result = result.checked_mul(10).ok_or(RationalError::Overflow)?;
    }
    Ok(result)
}

fn rational_seconds_to_ticks(seconds: Rational, round_up: bool) -> Result<u64, RationalError> {
    if seconds.denominator <= 0 {
        return Err(RationalError::NonPositiveDenominator);
    }
    if seconds.numerator < 0 {
        return Err(RationalError::NonFinite);
    }
    let ticks = seconds.checked_mul_u64(SIMULATION_HZ)?;
    let rounded = if round_up {
        ticks.ceil()
    } else {
        ticks.floor()
    };
    u64::try_from(rounded).map_err(|_| RationalError::Overflow)
}

/// Decompose a non-negative `f64` tick product into its floor and fraction.
///
/// The product is decomposed from the float bits instead of being converted
/// through a decimal spelling.  That keeps the conversion checked for large
/// finite values and lets the compatibility wrapper recognize the one
/// representable float nearest to an exact tick boundary (such as
/// `1.0 / 120.0`) without adding an epsilon that would move nearby values.
fn f64_seconds_to_tick_product(seconds: f64) -> Result<(u64, bool), RationalError> {
    let bits = seconds.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1u64 << 52) - 1);
    let (significand, shift) = if exponent == 0 {
        (fraction, -1074)
    } else {
        ((1u64 << 52) | fraction, exponent - 1023 - 52)
    };
    let scaled = u128::from(significand)
        .checked_mul(u128::from(SIMULATION_HZ))
        .ok_or(RationalError::Overflow)?;
    let (whole, fractional) = if shift >= 0 {
        let shift = u32::try_from(shift).map_err(|_| RationalError::Overflow)?;
        if shift >= 128 || scaled > u128::MAX >> shift {
            return Err(RationalError::Overflow);
        }
        (scaled << shift, false)
    } else {
        let shift = u32::try_from(-shift).map_err(|_| RationalError::Overflow)?;
        if shift >= 128 {
            (0, scaled != 0)
        } else {
            let mask = (1u128 << shift) - 1;
            (scaled >> shift, scaled & mask != 0)
        }
    };
    let ticks = u64::try_from(whole).map_err(|_| RationalError::Overflow)?;
    Ok((ticks, fractional))
}

const MAX_UNAMBIGUOUS_BOUNDARY_TICK: u64 = 1 << 52;

fn f64_seconds_to_boundary_tick(seconds: f64, floor_ticks: u64) -> Option<u64> {
    let hz = SIMULATION_HZ as f64;
    if floor_ticks <= MAX_UNAMBIGUOUS_BOUNDARY_TICK && seconds == floor_ticks as f64 / hz {
        return Some(floor_ticks);
    }
    let boundary_tick = floor_ticks.checked_add(1)?;
    (boundary_tick <= MAX_UNAMBIGUOUS_BOUNDARY_TICK && seconds == boundary_tick as f64 / hz)
        .then_some(boundary_tick)
}

fn f64_seconds_to_floor_ticks(seconds: f64) -> Result<u64, RationalError> {
    let (ticks, fractional) = f64_seconds_to_tick_product(seconds)?;
    if !fractional {
        return Ok(ticks);
    }

    // Below this limit adjacent 120-Hz boundaries are not collapsed into one
    // f64 value.  Equality with the correctly rounded boundary spelling is
    // therefore a precise compatibility check, not an epsilon adjustment.
    Ok(f64_seconds_to_boundary_tick(seconds, ticks).unwrap_or(ticks))
}

fn f64_seconds_to_ceil_ticks(seconds: f64) -> Result<u64, RationalError> {
    let (ticks, fractional) = f64_seconds_to_tick_product(seconds)?;
    if !fractional {
        return Ok(ticks);
    }

    // Treat the nearest representable spelling of an exact boundary as that
    // boundary.  Its predecessor still rounds up to the same tick, while its
    // successor is not equal to this spelling and therefore rounds upward.
    if let Some(boundary_tick) = f64_seconds_to_boundary_tick(seconds, ticks) {
        return Ok(boundary_tick);
    }
    ticks.checked_add(1).ok_or(RationalError::Overflow)
}

fn seconds_to_ticks(seconds: f64, round_up: bool) -> Result<u64, RationalError> {
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(RationalError::NonFinite);
    }
    if round_up {
        f64_seconds_to_ceil_ticks(seconds)
    } else {
        f64_seconds_to_floor_ticks(seconds)
    }
}

/// Rational repository-time clock sampled at exactly 120 Hz.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlaybackClock {
    pub tick: Tick,
    pub start_timestamp: i64,
    pub repository_offset: Rational,
    pub repository_rate: Rational,
}

pub type RationalClock = PlaybackClock;

impl PlaybackClock {
    /// Create a clock from an explicit repository-seconds-per-wall-second rate.
    pub fn new(start_timestamp: i64, repository_rate: Rational) -> Self {
        Self {
            tick: Tick::ZERO,
            start_timestamp,
            repository_offset: Rational::ZERO,
            repository_rate,
        }
    }

    /// Build the canonical rate from replay configuration.
    pub fn from_config(config: &ReplayConfig, start_timestamp: i64) -> Result<Self, ConfigError> {
        config.validate()?;
        let rate = if config.realtime {
            Rational::ONE
        } else {
            let seconds_per_day =
                Rational::from_f64(config.seconds_per_day).map_err(ConfigError::Rational)?;
            let time_scale =
                Rational::from_f64(config.time_scale).map_err(ConfigError::Rational)?;
            let day_seconds = Rational::from_u64(86_400);
            let base = Rational::new(
                day_seconds
                    .numerator
                    .checked_mul(seconds_per_day.denominator)
                    .ok_or(ConfigError::Rational(RationalError::Overflow))?,
                day_seconds
                    .denominator
                    .checked_mul(seconds_per_day.numerator)
                    .ok_or(ConfigError::Rational(RationalError::Overflow))?,
            )
            .map_err(ConfigError::Rational)?;
            base.checked_mul(time_scale)
                .map_err(ConfigError::Rational)?
        };
        Ok(Self::new(start_timestamp, rate))
    }

    /// Advance an integral number of canonical simulation ticks.
    pub fn advance_ticks(&mut self, ticks: u64) -> Result<(), RationalError> {
        let tick = self
            .tick
            .get()
            .checked_add(ticks)
            .ok_or(RationalError::Overflow)?;
        self.tick = Tick::new(tick).ok_or(RationalError::Overflow)?;
        let elapsed = self.repository_rate.checked_mul_u64(ticks)?.checked_mul(
            Rational::new(1, SIMULATION_HZ as i128).map_err(|_| RationalError::Overflow)?,
        )?;
        self.repository_offset = self.repository_offset.checked_add(elapsed)?;
        Ok(())
    }

    /// Advance to the floor of wall-seconds × 120 ticks.
    pub fn advance_wall_seconds(&mut self, seconds: f64) -> Result<Tick, RationalError> {
        let ticks = seconds_to_ticks(seconds, false)?;
        self.advance_ticks(ticks)?;
        Ok(self.tick)
    }

    /// Repository timestamp at the current tick, using floor semantics.
    pub fn repository_time(&self) -> i64 {
        let offset = self.repository_offset.floor();
        self.start_timestamp.saturating_add(offset as i64)
    }

    /// Exact repository-time value including subsecond progress.
    pub fn repository_rational(&self) -> Rational {
        Rational::new(
            (self.start_timestamp as i128)
                .saturating_mul(self.repository_offset.denominator)
                .saturating_add(self.repository_offset.numerator),
            self.repository_offset.denominator,
        )
        .unwrap_or(Rational::ZERO)
    }
    /// Convert an exact non-negative repository-seconds value to the
    /// canonical sampled tick without mutating state.
    pub fn sample_tick_rational(seconds: Rational) -> Result<Tick, RationalError> {
        let ticks = rational_seconds_to_ticks(seconds, false)?;
        Tick::new(ticks).ok_or(RationalError::Overflow)
    }

    /// Convert a wall/export time to the canonical sampled tick without
    /// mutating state.
    pub fn sample_tick(seconds: f64) -> Result<Tick, RationalError> {
        let ticks = seconds_to_ticks(seconds, false)?;
        Tick::new(ticks).ok_or(RationalError::Overflow)
    }
}

/// Validated replay-affecting configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayConfig {
    pub seconds_per_day: f64,
    pub realtime: bool,
    pub auto_skip_seconds: f64,
    pub time_scale: f64,
    pub file_idle_seconds: Option<f64>,
    pub camera_mode: CameraMode,
    pub seed: u64,
    pub algorithm_version: u32,
    pub limits: Limits,
}

pub type ReplayConfigV1 = ReplayConfig;

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            seconds_per_day: 10.0,
            realtime: false,
            auto_skip_seconds: 3.0,
            time_scale: 1.0,
            file_idle_seconds: None,
            camera_mode: CameraMode::Overview,
            seed: 31,
            algorithm_version: 1,
            limits: Limits::default(),
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum ConfigError {
    #[error("invalid or zero resource limit: {0}")]
    InvalidLimit(&'static str),
    #[error("configuration number is not finite: {0}")]
    NonFinite(&'static str),
    #[error("configuration value is out of range: {0}")]
    OutOfRange(&'static str),
    #[error("realtime conflicts with seconds_per_day")]
    RealtimeConflict,
    #[error("invalid camera mode: {0}")]
    InvalidCameraMode(String),
    #[error(transparent)]
    Rational(#[from] RationalError),
}

impl ReplayConfig {
    pub const VERSION: u16 = 1;

    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.seconds_per_day.is_finite() {
            return Err(ConfigError::NonFinite("seconds_per_day"));
        }
        if self.seconds_per_day <= 0.0 || self.seconds_per_day > 86_400.0 * 365.0 {
            return Err(ConfigError::OutOfRange("seconds_per_day"));
        }
        if !self.auto_skip_seconds.is_finite() || self.auto_skip_seconds < 0.0 {
            return Err(if self.auto_skip_seconds.is_finite() {
                ConfigError::OutOfRange("auto_skip_seconds")
            } else {
                ConfigError::NonFinite("auto_skip_seconds")
            });
        }
        if self.auto_skip_seconds > 86_400.0 * 365.0 {
            return Err(ConfigError::OutOfRange("auto_skip_seconds"));
        }
        if !self.time_scale.is_finite() || self.time_scale <= 0.0 || self.time_scale > 1_000_000.0 {
            return Err(if self.time_scale.is_finite() {
                ConfigError::OutOfRange("time_scale")
            } else {
                ConfigError::NonFinite("time_scale")
            });
        }
        if let Some(idle) = self.file_idle_seconds
            && (!idle.is_finite() || idle < 0.0 || idle > 86_400.0 * 365.0)
        {
            return Err(if idle.is_finite() {
                ConfigError::OutOfRange("file_idle_seconds")
            } else {
                ConfigError::NonFinite("file_idle_seconds")
            });
        }
        if self.realtime && (self.seconds_per_day - 86_400.0).abs() > f64::EPSILON {
            return Err(ConfigError::RealtimeConflict);
        }
        self.limits.validate()?;
        Ok(())
    }

    /// Convert the optional file-idle duration to canonical simulation ticks.
    ///
    /// `None` and zero disable file-idle expiry.  Positive durations round up
    /// so the configured duration is never shortened by tick quantization.
    pub fn file_idle_ticks(&self) -> Result<Option<u64>, ConfigError> {
        self.validate()?;
        let Some(seconds) = self.file_idle_seconds else {
            return Ok(None);
        };
        if seconds == 0.0 {
            return Ok(None);
        }
        seconds_to_ticks(seconds, true)
            .map(Some)
            .map_err(ConfigError::Rational)
    }

    pub fn validated(&self) -> Result<ValidatedReplayConfig, ConfigError> {
        self.validate()?;
        Ok(ValidatedReplayConfig(self.clone()))
    }
}

/// Marker wrapper proving replay configuration validation completed.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedReplayConfig(ReplayConfig);

impl std::ops::Deref for ValidatedReplayConfig {
    type Target = ReplayConfig;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Stable keyed deterministic perturbation.  It has no process-global state or
/// call-order dependency.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyedPerturbation {
    pub seed: u64,
    pub algorithm_version: u32,
}

impl KeyedPerturbation {
    pub const fn new(seed: u64, algorithm_version: u32) -> Self {
        Self {
            seed,
            algorithm_version,
        }
    }

    pub fn value(&self, key: &[u8]) -> [f64; 2] {
        keyed_perturbation(self.seed, self.algorithm_version, key)
    }
}

/// Derive two stable values in `[-1,1]` from an explicit seed/version/key.
pub fn keyed_perturbation(seed: u64, algorithm_version: u32, key: &[u8]) -> [f64; 2] {
    let mut state = seed ^ ((algorithm_version as u64) << 32) ^ (algorithm_version as u64);
    for byte in key {
        state ^= *byte as u64;
        state = splitmix64(state);
    }
    let first = splitmix64(state);
    let second = splitmix64(first);
    [unit_signed(first), unit_signed(second)]
}

pub fn keyed_hash(seed: u64, algorithm_version: u32, key: &[u8]) -> u64 {
    let values = keyed_perturbation(seed, algorithm_version, key);
    values[0].to_bits() ^ values[1].to_bits().rotate_left(17)
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut result = value;
    result = (result ^ (result >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    result = (result ^ (result >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    result ^ (result >> 31)
}

fn unit_signed(value: u64) -> f64 {
    ((value as f64 / u64::MAX as f64) * 2.0) - 1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(catalog: &mut Catalog, text: &str) -> PathId {
        let path = RepositoryPath::parse(text).unwrap();
        catalog.intern_path(&path).unwrap()
    }

    #[test]
    fn event_key_orders_timestamp_then_source_sequence() {
        let mut keys = [
            EventKey::new(2, SourceSeq::new(4).unwrap()),
            EventKey::new(1, SourceSeq::new(8).unwrap()),
            EventKey::new(2, SourceSeq::new(1).unwrap()),
        ];
        keys.sort();
        assert_eq!(keys[0].timestamp, 1);
        assert_eq!(keys[1].source_sequence.get(), 1);
        assert_eq!(keys[2].source_sequence.get(), 4);
    }

    #[test]
    fn strict_paths_keep_lexical_identity() {
        let ordinary = RepositoryPath::parse("/src\\main.rs").unwrap();
        assert_eq!(ordinary.canonical(), "src\\main.rs");
        assert!(!ordinary.is_directory());
        assert_eq!(
            RepositoryPath::parse("src/main.rs/").unwrap().canonical(),
            "src/main.rs/"
        );
        for invalid in ["", "/", "a//b", "a/./b", "a/../b", "a\0b", "a\nb"] {
            assert!(RepositoryPath::parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn path_component_limits_reject_overflow_before_push() {
        let exact = RepositoryPath::parse_with_limits("/a/b/c/", 1024, 3).unwrap();
        assert_eq!(exact.canonical(), "a/b/c/");
        assert_eq!(
            exact.components(),
            &["a".to_owned(), "b".to_owned(), "c".to_owned()]
        );

        assert_eq!(
            RepositoryPath::parse_with_limits("a/b/c/d", 1024, 3),
            Err(PathError::TooDeep { limit: 3 })
        );
        assert_eq!(
            RepositoryPath::parse_with_limits("a", 1024, 0),
            Err(PathError::TooDeep { limit: 0 })
        );
        assert_eq!(
            RepositoryPath::parse_with_limits("/", 1024, 0),
            Err(PathError::Empty)
        );
    }

    #[test]
    fn compressed_world_splits_and_prunes_components() {
        let mut catalog = Catalog::new();
        let first = path(&mut catalog, "src/lib/a.rs");
        let second = path(&mut catalog, "src/main.rs");
        let alice = catalog.intern_contributor("alice").unwrap();
        let mut world = World::new();
        world
            .apply_event(
                &Event::new(
                    EventKey::new(0, SourceSeq::new(0).unwrap()),
                    Generation::ZERO,
                    alice,
                    EventTarget::File(first),
                    Action::Add,
                    None,
                ),
                &catalog,
            )
            .unwrap();
        world
            .apply_event(
                &Event::new(
                    EventKey::new(1, SourceSeq::new(1).unwrap()),
                    Generation::ZERO,
                    alice,
                    EventTarget::File(second),
                    Action::Add,
                    None,
                ),
                &catalog,
            )
            .unwrap();
        assert_eq!(world.active_files().len(), 2);
        assert!(
            world
                .directories()
                .iter()
                .all(|dir| !dir.label.iter().any(String::is_empty))
        );
        let delete = Event::new(
            EventKey::new(2, SourceSeq::new(2).unwrap()),
            Generation::ZERO,
            alice,
            EventTarget::File(first),
            Action::Delete,
            None,
        );
        world.apply_event(&delete, &catalog).unwrap();
        assert_eq!(world.active_files().len(), 1);
    }

    #[test]
    fn file_to_directory_conversion_and_recreation_use_new_ids() {
        let mut catalog = Catalog::new();
        let old = path(&mut catalog, "a");
        let descendant = path(&mut catalog, "a/b");
        let contributor = catalog.intern_contributor("alice").unwrap();
        let mut world = World::new();
        let add_old = Event::new(
            EventKey::new(0, SourceSeq::new(0).unwrap()),
            Generation::ZERO,
            contributor,
            EventTarget::File(old),
            Action::Add,
            None,
        );
        let first = world.apply_event(&add_old, &catalog).unwrap().created_files[0];
        let add_descendant = Event::new(
            EventKey::new(1, SourceSeq::new(1).unwrap()),
            Generation::ZERO,
            contributor,
            EventTarget::File(descendant),
            Action::Add,
            None,
        );
        let second = world
            .apply_event(&add_descendant, &catalog)
            .unwrap()
            .created_files[0];
        assert_ne!(first, second);
        assert!(!world.file(first).unwrap().alive);
        world
            .apply_event(
                &Event::new(
                    EventKey::new(2, SourceSeq::new(2).unwrap()),
                    Generation::ZERO,
                    contributor,
                    EventTarget::File(descendant),
                    Action::Delete,
                    None,
                ),
                &catalog,
            )
            .unwrap();
        let recreated = world
            .apply_event(
                &Event::new(
                    EventKey::new(3, SourceSeq::new(3).unwrap()),
                    Generation::ZERO,
                    contributor,
                    EventTarget::File(descendant),
                    Action::Add,
                    None,
                ),
                &catalog,
            )
            .unwrap()
            .created_files[0];
        assert_ne!(second, recreated);
    }

    #[test]
    fn absent_delete_is_activity_without_topology() {
        let mut catalog = Catalog::new();
        let missing = path(&mut catalog, "missing.txt");
        let contributor = catalog.intern_contributor("alice").unwrap();
        let mut world = World::new();
        let delta = world
            .apply_event(
                &Event::new(
                    EventKey::new(0, SourceSeq::new(0).unwrap()),
                    Generation::ZERO,
                    contributor,
                    EventTarget::File(missing),
                    Action::Delete,
                    None,
                ),
                &catalog,
            )
            .unwrap();
        assert!(delta.created_files.is_empty());
        assert_eq!(delta.activities.len(), 1);
        assert!(delta.activities[0].file_id.is_none());
        assert!(world.active_files().is_empty());
    }

    #[test]
    fn directory_delete_expands_lexically() {
        let mut catalog = Catalog::new();
        let one = path(&mut catalog, "dir/z");
        let two = path(&mut catalog, "dir/a");
        let target_path = path(&mut catalog, "dir/");
        let contributor = catalog.intern_contributor("alice").unwrap();
        let mut world = World::new();
        for (seq, target) in [(0, one), (1, two)] {
            world
                .apply_event(
                    &Event::new(
                        EventKey::new(seq as i64, SourceSeq::new(seq).unwrap()),
                        Generation::ZERO,
                        contributor,
                        EventTarget::File(target),
                        Action::Add,
                        None,
                    ),
                    &catalog,
                )
                .unwrap();
        }
        let delta = world
            .apply_event(
                &Event::new(
                    EventKey::new(2, SourceSeq::new(2).unwrap()),
                    Generation::ZERO,
                    contributor,
                    EventTarget::Directory(target_path),
                    Action::Delete,
                    None,
                ),
                &catalog,
            )
            .unwrap();
        assert_eq!(delta.deleted_files.len(), 2);
        let deleted_paths: Vec<_> = delta
            .deleted_files
            .iter()
            .map(|file_id| {
                let file = world
                    .file(*file_id)
                    .expect("deleted file remains addressable by ID");
                catalog
                    .path(file.path_id)
                    .expect("deleted file path remains in catalog")
                    .canonical()
            })
            .collect();
        assert_eq!(deleted_paths, ["dir/a", "dir/z"]);
        assert!(world.active_files().is_empty());
    }

    #[test]
    fn file_idle_ticks_none_and_zero_disable_expiry() {
        let mut config = ReplayConfig::default();
        assert_eq!(config.file_idle_ticks(), Ok(None));

        config.file_idle_seconds = Some(0.0);
        assert_eq!(config.file_idle_ticks(), Ok(None));

        config.file_idle_seconds = Some(-0.0);
        assert_eq!(config.file_idle_ticks(), Ok(None));
    }

    #[test]
    fn file_idle_ticks_ceil_fractional_and_exact_boundaries() {
        let hz = SIMULATION_HZ as f64;
        let boundary = 1.0 / hz;
        let predecessor = f64::from_bits(boundary.to_bits() - 1);
        let successor = f64::from_bits(boundary.to_bits() + 1);
        let boundary_119 = 119.0 / hz;
        let predecessor_119 = f64::from_bits(boundary_119.to_bits() - 1);
        let successor_119 = f64::from_bits(boundary_119.to_bits() + 1);
        let mut config = ReplayConfig {
            file_idle_seconds: Some(0.01),
            ..ReplayConfig::default()
        };
        assert_eq!(config.file_idle_ticks(), Ok(Some(2)));

        config.file_idle_seconds = Some(predecessor);
        assert_eq!(config.file_idle_ticks(), Ok(Some(1)));
        config.file_idle_seconds = Some(boundary);
        assert_eq!(config.file_idle_ticks(), Ok(Some(1)));
        config.file_idle_seconds = Some(successor);
        assert_eq!(config.file_idle_ticks(), Ok(Some(2)));

        config.file_idle_seconds = Some(predecessor_119);
        assert_eq!(config.file_idle_ticks(), Ok(Some(119)));
        config.file_idle_seconds = Some(boundary_119);
        assert_eq!(config.file_idle_ticks(), Ok(Some(119)));
        config.file_idle_seconds = Some(successor_119);
        assert_eq!(config.file_idle_ticks(), Ok(Some(120)));

        config.file_idle_seconds = Some(1.0 / (2.0 * hz));
        assert_eq!(config.file_idle_ticks(), Ok(Some(1)));
        config.file_idle_seconds = Some(1.0);
        assert_eq!(config.file_idle_ticks(), Ok(Some(SIMULATION_HZ)));
        config.file_idle_seconds = Some(86_400.0 * 365.0);
        assert_eq!(config.file_idle_ticks(), Ok(Some(3_784_320_000)));
    }

    #[test]
    fn file_idle_ticks_rejects_invalid_and_overflowing_values() {
        let mut config = ReplayConfig {
            file_idle_seconds: Some(f64::NAN),
            ..ReplayConfig::default()
        };
        assert_eq!(
            config.file_idle_ticks(),
            Err(ConfigError::NonFinite("file_idle_seconds"))
        );

        config.file_idle_seconds = Some(-1.0);
        assert_eq!(
            config.file_idle_ticks(),
            Err(ConfigError::OutOfRange("file_idle_seconds"))
        );

        config.file_idle_seconds = Some(f64::MAX);
        assert_eq!(
            config.file_idle_ticks(),
            Err(ConfigError::OutOfRange("file_idle_seconds"))
        );
        assert_eq!(
            PlaybackClock::sample_tick(f64::MAX),
            Err(RationalError::Overflow)
        );
        let too_large = 2.0_f64.powi(125);
        assert_eq!(
            seconds_to_ticks(too_large, false),
            Err(RationalError::Overflow)
        );
        assert_eq!(
            seconds_to_ticks(too_large, true),
            Err(RationalError::Overflow)
        );
        assert_eq!(
            PlaybackClock::sample_tick(-f64::MIN_POSITIVE),
            Err(RationalError::NonFinite)
        );
        assert_eq!(
            PlaybackClock::sample_tick_rational(Rational::new(-1, SIMULATION_HZ as i128).unwrap()),
            Err(RationalError::NonFinite)
        );
    }

    #[test]
    fn rational_ceil_handles_i128_boundaries() {
        assert_eq!(Rational::new(i128::MIN, 1).unwrap().ceil(), i128::MIN);
        assert_eq!(Rational::new(i128::MIN, i128::MAX).unwrap().ceil(), -1);
        assert_eq!(
            Rational::new(i128::MAX, 2).unwrap().ceil(),
            i128::MAX / 2 + 1
        );
    }

    #[test]
    fn clock_uses_exact_120_hz_ticks_and_floor_sampling() {
        let config = ReplayConfig::default();
        assert_eq!(config.seconds_per_day, 10.0);
        let start_timestamp = 1_000;
        let mut clock = PlaybackClock::from_config(&config, start_timestamp).unwrap();
        let seconds_per_day = Rational::from_f64(config.seconds_per_day).unwrap();
        let expected_rate = Rational::new(
            86_400i128 * seconds_per_day.denominator,
            seconds_per_day.numerator,
        )
        .unwrap();
        let tick_duration = Rational::new(1, SIMULATION_HZ as i128).unwrap();

        assert_eq!(clock.repository_rate, expected_rate);

        let ticks_before_boundary = SIMULATION_HZ - 1;
        clock.advance_ticks(ticks_before_boundary).unwrap();
        let expected_elapsed = expected_rate
            .checked_mul_u64(ticks_before_boundary)
            .unwrap()
            .checked_mul(tick_duration)
            .unwrap();
        let expected_repository = Rational::new(
            (start_timestamp as i128)
                .checked_mul(expected_elapsed.denominator)
                .unwrap()
                .checked_add(expected_elapsed.numerator)
                .unwrap(),
            expected_elapsed.denominator,
        )
        .unwrap();
        assert_eq!(clock.repository_rational(), expected_repository);
        assert_eq!(
            clock.repository_time(),
            start_timestamp + expected_elapsed.floor() as i64
        );

        clock.advance_ticks(1).unwrap();
        assert_eq!(clock.tick.get(), SIMULATION_HZ);
        let expected_at_boundary = expected_rate
            .checked_mul_u64(SIMULATION_HZ)
            .unwrap()
            .checked_mul(tick_duration)
            .unwrap();
        assert_eq!(
            clock.repository_time(),
            start_timestamp + expected_at_boundary.floor() as i64
        );

        let just_before_tick = f64::from_bits((1.0 / SIMULATION_HZ as f64).to_bits() - 1);
        assert_eq!(
            PlaybackClock::sample_tick(just_before_tick).unwrap().get(),
            0
        );
        assert_eq!(
            PlaybackClock::sample_tick(1.0 / SIMULATION_HZ as f64)
                .unwrap()
                .get(),
            1
        );
        assert_eq!(
            PlaybackClock::sample_tick(119.0 / SIMULATION_HZ as f64)
                .unwrap()
                .get(),
            119
        );
        assert_eq!(
            PlaybackClock::sample_tick(1_000_000_000_000.0)
                .unwrap()
                .get(),
            120_000_000_000_000
        );
    }

    #[test]
    fn rational_sampling_preserves_floor_boundaries() {
        let exact = Rational::new(1, SIMULATION_HZ as i128).unwrap();
        let just_before = Rational::new(1, SIMULATION_HZ as i128 + 1).unwrap();
        assert_eq!(
            PlaybackClock::sample_tick_rational(just_before)
                .unwrap()
                .get(),
            0
        );
        assert_eq!(PlaybackClock::sample_tick_rational(exact).unwrap().get(), 1);
        assert_eq!(
            PlaybackClock::sample_tick_rational(Rational::new(119, SIMULATION_HZ as i128).unwrap())
                .unwrap()
                .get(),
            119
        );
    }

    #[test]
    fn empty_history_is_valid() {
        let history = History::empty();
        assert!(history.is_empty());
        assert_eq!(history.event_count(), 0);
        assert!(history.event(EventIndex::ZERO).is_none());
    }

    #[test]
    fn keyed_perturbations_are_repeatable_and_keyed() {
        let one = keyed_perturbation(31, 1, b"src/a");
        let two = keyed_perturbation(31, 1, b"src/a");
        let three = keyed_perturbation(31, 1, b"src/b");
        assert_eq!(one, two);
        assert_ne!(one, three);
    }
}
