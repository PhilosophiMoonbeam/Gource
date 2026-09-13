// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Versioned metadata for reproducible export artifacts.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gource_core::{HistorySource, ReplayConfig};
use serde::{Deserialize, Serialize};

use crate::schedule::{ExportSchedule, FrameRate};

const MAX_STAGING_ATTEMPTS: usize = 32;
static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Current export-manifest schema.
pub const EXPORT_MANIFEST_VERSION: u16 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InputIdentity {
    pub schema: u16,
    pub digest: String,
    pub event_count: u64,
    pub path_count: u64,
    pub contributor_count: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExportConfigIdentity {
    pub width: u32,
    pub height: u32,
    pub frame_rate: FrameRate,
    pub start: gource_core::Rational,
    pub end: gource_core::Rational,
    pub replay: ReplayConfig,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolchainIdentity {
    pub rustc: String,
    pub package_version: String,
    pub revision: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BackendIdentity {
    pub backend: String,
    pub adapter: String,
    pub driver: String,
    pub driver_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RenderIdentity {
    pub target_format: String,
    pub transfer: String,
    pub blend: String,
    pub sample_count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EncoderIdentity {
    pub kind: String,
    pub executable: Option<String>,
    pub codec: Option<String>,
    pub container: Option<String>,
    pub pixel_format: String,
    pub color_space: String,
    pub color_primaries: String,
    pub color_transfer: String,
    pub color_range: String,
}

/// Metadata emitted for every successful export.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExportManifestV1 {
    pub schema: u16,
    pub input: InputIdentity,
    pub config: ExportConfigIdentity,
    pub seed: u64,
    pub revision: String,
    pub toolchain: ToolchainIdentity,
    pub backend: BackendIdentity,
    pub render: RenderIdentity,
    pub encoder: EncoderIdentity,
    pub frame_count: u64,
}

impl ExportManifestV1 {
    pub fn from_history<H: HistorySource>(
        history: &H,
        width: u32,
        height: u32,
        schedule: &ExportSchedule,
        replay: &ReplayConfig,
        backend: BackendIdentity,
        encoder: EncoderIdentity,
    ) -> Self {
        let input = input_identity(history);
        let revision = option_env!("GOURCE_REVISION")
            .unwrap_or("unknown")
            .to_owned();
        let toolchain = ToolchainIdentity {
            rustc: option_env!("RUSTC_VERSION").unwrap_or("unknown").to_owned(),
            package_version: env!("CARGO_PKG_VERSION").to_owned(),
            revision: revision.clone(),
        };
        Self {
            schema: EXPORT_MANIFEST_VERSION,
            input,
            config: ExportConfigIdentity {
                width,
                height,
                frame_rate: schedule.frame_rate,
                start: schedule.start,
                end: schedule.end,
                replay: replay.clone(),
            },
            seed: replay.seed,
            revision,
            toolchain,
            backend,
            render: RenderIdentity {
                target_format: "Rgba8Unorm".to_owned(),
                transfer: "gamma-space".to_owned(),
                blend: "premultiplied-one-one-minus-src-alpha".to_owned(),
                sample_count: 1,
            },
            encoder,
            frame_count: schedule.frame_count(),
        }
    }

    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Atomically publish a manifest file next to an already-published output.
    pub fn write_atomic(&self, path: impl AsRef<Path>) -> Result<PathBuf, ManifestError> {
        let path = path.as_ref();
        let initial_sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.write_atomic_with_staging_sequence(path, initial_sequence)
    }

    #[cfg(all(test, unix))]
    fn write_atomic_for_test(
        &self,
        path: impl AsRef<Path>,
        initial_sequence: u64,
    ) -> Result<PathBuf, ManifestError> {
        self.write_atomic_with_staging_sequence(path.as_ref(), initial_sequence)
    }

    fn write_atomic_with_staging_sequence(
        &self,
        path: &Path,
        initial_sequence: u64,
    ) -> Result<PathBuf, ManifestError> {
        let parent = path
            .parent()
            .ok_or_else(|| ManifestError::InvalidPath(path.to_owned()))?;
        fs::create_dir_all(parent).map_err(ManifestError::Io)?;
        let text = self.to_toml().map_err(ManifestError::Serialize)?;
        let (temporary, mut file) =
            create_staging_file(path, initial_sequence).map_err(ManifestError::Io)?;

        let write_result = (|| -> io::Result<()> {
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = write_result {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(ManifestError::Io(error));
        }
        drop(file);

        if let Err(error) = fs::rename(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(ManifestError::Io(error));
        }
        sync_parent(parent).map_err(ManifestError::Io)?;
        Ok(path.to_owned())
    }
}

fn create_staging_file(path: &Path, initial_sequence: u64) -> io::Result<(PathBuf, fs::File)> {
    let process_id = std::process::id();
    for offset in 0..MAX_STAGING_ATTEMPTS {
        let sequence = initial_sequence.wrapping_add(offset as u64);
        let temporary = path.with_extension(format!("partial-{process_id}-{sequence}"));
        // `create_new` rejects an existing final symlink and returns a handle
        // to the newly created file, so all writes stay on this inode.
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        match options.open(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "unable to allocate a unique manifest staging path",
    ))
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> io::Result<()> {
    fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> io::Result<()> {
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest path is invalid: {0}")]
    InvalidPath(PathBuf),
    #[error("manifest I/O failed: {0}")]
    Io(#[source] std::io::Error),
    #[error("manifest serialization failed: {0}")]
    Serialize(#[source] toml::ser::Error),
}

fn input_identity<H: HistorySource>(history: &H) -> InputIdentity {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&history.catalog().version().to_le_bytes());
    for path in history.catalog().paths() {
        hasher.update(path.canonical().as_bytes());
        hasher.update(&[0]);
    }
    for contributor in history.catalog().contributors() {
        hasher.update(contributor.as_bytes());
        hasher.update(&[0]);
    }
    for event in history.events() {
        hasher.update(format!("{event:?}").as_bytes());
        hasher.update(&[0]);
    }
    InputIdentity {
        schema: gource_core::HISTORY_SCHEMA_VERSION,
        digest: hasher.finalize().to_hex().to_string(),
        event_count: history.events().len() as u64,
        path_count: history.catalog().path_count() as u64,
        contributor_count: history.catalog().contributor_count() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gource_core::History;

    fn manifest_for_test() -> ExportManifestV1 {
        let history = History::empty();
        let rate = FrameRate::integer(60).unwrap();
        let schedule = ExportSchedule::new(
            gource_core::Rational::ZERO,
            gource_core::Rational::ONE,
            rate,
        )
        .unwrap();
        let replay = ReplayConfig::default();
        ExportManifestV1::from_history(
            &history,
            3,
            2,
            &schedule,
            &replay,
            BackendIdentity {
                backend: "test".to_owned(),
                adapter: "adapter".to_owned(),
                driver: "driver".to_owned(),
                driver_version: "1".to_owned(),
            },
            EncoderIdentity {
                kind: "frames".to_owned(),
                executable: None,
                codec: None,
                container: None,
                pixel_format: "rgba".to_owned(),
                color_space: "bt709".to_owned(),
                color_primaries: "bt709".to_owned(),
                color_transfer: "bt709".to_owned(),
                color_range: "tv".to_owned(),
            },
        )
    }

    #[test]
    fn manifest_contains_requested_identity_sections() {
        let manifest = manifest_for_test();
        let text = manifest.to_toml().unwrap();
        assert!(text.contains("[input]"));
        assert!(text.contains("[backend]"));
        assert!(text.contains("[encoder]"));
        assert_eq!(manifest.frame_count, 60);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_manifest_does_not_follow_existing_staging_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manifest.toml");
        let legacy_victim = directory.path().join("legacy-victim");
        let exact_victim = directory.path().join("exact-victim");
        fs::write(&legacy_victim, b"preserve legacy").unwrap();
        fs::write(&exact_victim, b"preserve exact").unwrap();

        let process_id = std::process::id();
        let initial_sequence = 41;
        let predictable = path.with_extension(format!("partial-{process_id}"));
        let exact = path.with_extension(format!("partial-{process_id}-{initial_sequence}"));
        std::os::unix::fs::symlink(&legacy_victim, &predictable).unwrap();
        std::os::unix::fs::symlink(&exact_victim, &exact).unwrap();

        let manifest = manifest_for_test();
        let expected = manifest.to_toml().unwrap();
        manifest
            .write_atomic_for_test(&path, initial_sequence)
            .unwrap();

        assert_eq!(fs::read(&legacy_victim).unwrap(), b"preserve legacy");
        assert_eq!(fs::read(&exact_victim).unwrap(), b"preserve exact");
        assert_eq!(fs::read_to_string(&path).unwrap(), expected);
        assert_eq!(fs::read_link(&predictable).unwrap(), legacy_victim);
        assert_eq!(fs::read_link(&exact).unwrap(), exact_victim);
    }
}
