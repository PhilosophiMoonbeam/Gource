// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Replay-driven headless export orchestration.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::manifest::{BackendIdentity, EncoderIdentity, ExportManifestV1, ManifestError};
use crate::readback::{GpuReadback, ReadbackError, ReadbackLayout, ReadbackSlotPool};
use crate::schedule::{ExportSchedule, FrameRate, ScheduleError};
use crate::sink::{FfmpegConfig, FfmpegSink, FrameSink, PngFrameSink, SinkError};
use gource_core::{CameraMode, HistorySource, Rational, ReplayConfig};
use gource_render::{GpuContext, RENDER_TARGET_FORMAT, RenderView, RendererConfig, SceneRenderer};
use gource_sim::{ReplayError, ReplaySession, SceneSnapshot};

/// Output target selected by the CLI/application.
#[derive(Clone, Debug)]
pub enum ExportOutput {
    Frames(PathBuf),
    Video(FfmpegConfig),
}

impl ExportOutput {
    #[must_use]
    pub fn frames(path: impl Into<PathBuf>) -> Self {
        Self::Frames(path.into())
    }

    #[must_use]
    pub fn video(config: FfmpegConfig) -> Self {
        Self::Video(config)
    }

    #[must_use]
    pub fn output_path(&self) -> &Path {
        match self {
            Self::Frames(path) => path,
            Self::Video(config) => &config.output,
        }
    }
}

/// Validated export dimensions, schedule, replay settings, and bounded
/// buffering policy.
#[derive(Clone, Debug)]
pub struct ExportSpec {
    pub width: u32,
    pub height: u32,
    pub frame_rate: FrameRate,
    pub start: Rational,
    pub end: Rational,
    pub output: ExportOutput,
    pub replay: ReplayConfig,
    pub label_budget: usize,
    pub clear_color: [f64; 4],
    pub readback_slots: usize,
    pub max_readback_bytes: u64,
}

impl Default for ExportSpec {
    fn default() -> Self {
        let width = 1280;
        let height = 720;
        let frame_rate = FrameRate::default();
        let readback_slots = 2;
        let layout = ReadbackLayout::rgba8(width, height)
            .expect("default export dimensions must remain valid");
        let max_readback_bytes = checked_readback_bytes(layout, readback_slots)
            .expect("default export readback cap must fit");
        Self {
            width,
            height,
            frame_rate,
            start: Rational::ZERO,
            end: Rational::ZERO,
            output: ExportOutput::Frames(PathBuf::from("frames")),
            replay: ReplayConfig::default(),
            label_budget: 4096,
            clear_color: [0.015, 0.02, 0.03, 1.0],
            readback_slots,
            max_readback_bytes,
        }
    }
}

fn checked_readback_bytes(layout: ReadbackLayout, slots: usize) -> Result<u64, ExportError> {
    let bytes = u64::try_from(layout.byte_len)
        .map_err(|_| ExportError::InvalidSpec("readback byte cap overflow"))?;
    let slots = u64::try_from(slots)
        .map_err(|_| ExportError::InvalidSpec("readback slot count overflow"))?;
    bytes
        .checked_mul(slots)
        .ok_or(ExportError::InvalidSpec("readback byte cap overflow"))
}

impl ExportSpec {
    pub fn new(
        width: u32,
        height: u32,
        frame_rate: FrameRate,
        start: Rational,
        end: Rational,
        output: ExportOutput,
        replay: ReplayConfig,
    ) -> Result<Self, ExportError> {
        let mut spec = Self {
            width,
            height,
            frame_rate,
            start,
            end,
            output,
            replay,
            ..Self::default()
        };
        if width != 0 && height != 0 && spec.readback_slots != 0 {
            let layout = ReadbackLayout::rgba8(width, height)?;
            spec.max_readback_bytes = checked_readback_bytes(layout, spec.readback_slots)?;
        }
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<(), ExportError> {
        if self.width == 0 || self.height == 0 {
            return Err(ExportError::InvalidSpec(
                "width and height must be non-zero",
            ));
        }
        if self.readback_slots == 0 {
            return Err(ExportError::InvalidSpec("readback_slots must be positive"));
        }
        if self.label_budget == 0 {
            return Err(ExportError::InvalidSpec("label_budget must be positive"));
        }
        self.replay
            .validate()
            .map_err(|_| ExportError::InvalidSpec("replay configuration rejected"))?;
        let _ = ExportSchedule::new(self.start, self.end, self.frame_rate)?;
        let layout = ReadbackLayout::rgba8(self.width, self.height)?;
        let required = checked_readback_bytes(layout, self.readback_slots)?;
        if required > self.max_readback_bytes {
            return Err(ExportError::InvalidSpec("readback byte cap is too small"));
        }
        match &self.output {
            ExportOutput::Frames(_) => {}
            ExportOutput::Video(config) => {
                if config.width != self.width || config.height != self.height {
                    return Err(ExportError::InvalidSpec(
                        "video dimensions must match export dimensions",
                    ));
                }
                if config.frame_rate != self.frame_rate {
                    return Err(ExportError::InvalidSpec(
                        "video frame rate must match export frame rate",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn schedule(&self) -> Result<ExportSchedule, ExportError> {
        Ok(ExportSchedule::new(self.start, self.end, self.frame_rate)?)
    }

    #[must_use]
    pub fn start_time(&self) -> Rational {
        self.start
    }

    #[must_use]
    pub fn end_time(&self) -> Rational {
        self.end
    }

    pub fn frame_count(&self) -> Result<u64, ExportError> {
        Ok(self.schedule()?.frame_count())
    }
}

/// Aggregate export failure.  A failed job never publishes a success artifact.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("invalid export specification: {0}")]
    InvalidSpec(&'static str),
    #[error(
        "export texture extent {width}x{height} exceeds selected adapter maximum texture dimension {maximum}"
    )]
    TextureExtentLimit {
        width: u32,
        height: u32,
        maximum: u32,
    },
    #[error(transparent)]
    Schedule(#[from] ScheduleError),
    #[error(transparent)]
    Readback(#[from] ReadbackError),
    #[error(transparent)]
    Replay(#[from] ReplayError),
    #[error("GPU context request failed: {0}")]
    Gpu(#[from] gource_render::GpuContextError),
    #[error("renderer failed: {0}")]
    Renderer(#[from] gource_render::RendererError),
    #[error(transparent)]
    Sink(#[from] SinkError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("export cancelled")]
    Cancelled,
}

/// Result of a successful export.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportReport {
    pub frames: u64,
    pub bytes: u64,
    pub output: PathBuf,
    pub manifest: PathBuf,
}

/// Owns one replay session and drives it for every scheduled output frame.
pub struct ExportJob<H: HistorySource + Clone> {
    history: H,
    spec: ExportSpec,
    context: Option<GpuContext>,
    cancellation: Arc<AtomicBool>,
}

struct RenderParams<'a> {
    cancellation: &'a AtomicBool,
    width: u32,
    height: u32,
    schedule: &'a ExportSchedule,
    context: &'a GpuContext,
    texture: &'a wgpu::Texture,
}

impl<H: HistorySource + Clone> ExportJob<H> {
    #[must_use]
    pub fn new(history: H, spec: ExportSpec) -> Self {
        Self {
            history,
            spec,
            context: None,
            cancellation: Arc::new(AtomicBool::new(false)),
        }
    }
    #[must_use]
    pub fn with_context(history: H, spec: ExportSpec, context: GpuContext) -> Self {
        Self {
            history,
            spec,
            context: Some(context),
            cancellation: Arc::new(AtomicBool::new(false)),
        }
    }

    #[must_use]
    pub fn spec(&self) -> &ExportSpec {
        &self.spec
    }

    #[must_use]
    pub fn cancellation(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancellation)
    }

    pub fn cancel(&self) {
        self.cancellation.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.load(Ordering::Acquire)
    }

    /// Run a headless export.  A context is requested lazily so an empty range
    /// remains usable on machines without a graphics adapter.
    pub fn run(&mut self) -> Result<ExportReport, ExportError> {
        self.spec.validate()?;
        let schedule = self.spec.schedule()?;
        if self.is_cancelled() {
            return Err(ExportError::Cancelled);
        }
        let layout = ReadbackLayout::rgba8(self.spec.width, self.spec.height)?;
        let _slots = ReadbackSlotPool::new(
            layout,
            self.spec.readback_slots,
            self.spec.max_readback_bytes,
        )?;
        match self.spec.output.clone() {
            ExportOutput::Frames(path) => self.run_frames(&schedule, layout, &path),
            ExportOutput::Video(config) => self.run_video(&schedule, layout, &config),
        }
    }

    fn run_frames(
        &mut self,
        schedule: &ExportSchedule,
        layout: ReadbackLayout,
        path: &Path,
    ) -> Result<ExportReport, ExportError> {
        let mut sink = PngFrameSink::new(path, self.spec.width, self.spec.height)?;
        if schedule.is_empty() {
            let manifest = ExportManifestV1::from_history(
                &self.history,
                self.spec.width,
                self.spec.height,
                schedule,
                &self.spec.replay,
                self.backend_identity(),
                EncoderIdentity::frames(),
            );
            sink.write_manifest(
                &manifest
                    .to_toml()
                    .map_err(crate::manifest::ManifestError::Serialize)?,
            )?;
            let report = sink.finish()?;
            return Ok(ExportReport {
                frames: report.frames,
                bytes: report.bytes,
                output: report.output,
                manifest: path.join("manifest.toml"),
            });
        }
        if let Err(error) = self.ensure_context() {
            sink.cancel();
            return Err(error);
        }
        let manifest = ExportManifestV1::from_history(
            &self.history,
            self.spec.width,
            self.spec.height,
            schedule,
            &self.spec.replay,
            self.backend_identity(),
            EncoderIdentity::frames(),
        );
        sink.write_manifest(
            &manifest
                .to_toml()
                .map_err(crate::manifest::ManifestError::Serialize)?,
        )?;
        let mut session = ReplaySession::new(self.history.clone(), self.spec.replay.clone())?;
        let context = self
            .context
            .as_ref()
            .ok_or(ExportError::InvalidSpec("GPU context unavailable"))?;
        let mut renderer = Self::make_renderer(&self.spec, context)?;
        let texture = make_offscreen_texture(context, self.spec.width, self.spec.height);
        let mut readback =
            GpuReadback::new(context.device(), layout, Some("gource-export-readback"))?;
        let result = Self::render_into_sink(
            RenderParams {
                cancellation: &self.cancellation,
                width: self.spec.width,
                height: self.spec.height,
                schedule,
                context,
                texture: &texture,
            },
            &mut session,
            &mut renderer,
            &mut readback,
            &mut sink,
        );
        match result {
            Ok(()) => {
                let report = sink.finish()?;
                Ok(ExportReport {
                    frames: report.frames,
                    bytes: report.bytes,
                    output: report.output,
                    manifest: path.join("manifest.toml"),
                })
            }
            Err(error) => {
                sink.cancel();
                Err(error)
            }
        }
    }

    fn run_video(
        &mut self,
        schedule: &ExportSchedule,
        layout: ReadbackLayout,
        config: &FfmpegConfig,
    ) -> Result<ExportReport, ExportError> {
        let mut sink =
            FfmpegSink::spawn_with_cancellation(config.clone(), Arc::clone(&self.cancellation))?;
        if schedule.is_empty() {
            let manifest = ExportManifestV1::from_history(
                &self.history,
                self.spec.width,
                self.spec.height,
                schedule,
                &self.spec.replay,
                self.backend_identity(),
                EncoderIdentity::ffmpeg(config),
            );
            let report = sink.finish()?;
            let manifest_path = config.output.with_extension("manifest.toml");
            if let Err(error) = manifest.write_atomic(&manifest_path) {
                let _ = std::fs::remove_file(&config.output);
                return Err(error.into());
            }
            return Ok(ExportReport {
                frames: report.frames,
                bytes: report.bytes,
                output: report.output,
                manifest: manifest_path,
            });
        }
        if let Err(error) = self.ensure_context() {
            sink.cancel();
            return Err(error);
        }
        let manifest = ExportManifestV1::from_history(
            &self.history,
            self.spec.width,
            self.spec.height,
            schedule,
            &self.spec.replay,
            self.backend_identity(),
            EncoderIdentity::ffmpeg(config),
        );
        let mut session = match ReplaySession::new(self.history.clone(), self.spec.replay.clone()) {
            Ok(session) => session,
            Err(error) => {
                sink.cancel();
                return Err(error.into());
            }
        };
        let context = self
            .context
            .as_ref()
            .ok_or(ExportError::InvalidSpec("GPU context unavailable"))?;
        let mut renderer = match Self::make_renderer(&self.spec, context) {
            Ok(renderer) => renderer,
            Err(error) => {
                sink.cancel();
                return Err(error);
            }
        };
        let texture = make_offscreen_texture(context, self.spec.width, self.spec.height);
        let mut readback =
            GpuReadback::new(context.device(), layout, Some("gource-export-readback"))?;
        let result = Self::render_into_sink(
            RenderParams {
                cancellation: &self.cancellation,
                width: self.spec.width,
                height: self.spec.height,
                schedule,
                context,
                texture: &texture,
            },
            &mut session,
            &mut renderer,
            &mut readback,
            &mut sink,
        );
        if let Err(error) = result {
            sink.cancel();
            return Err(error);
        }
        let report = sink.finish()?;
        let manifest_path = config.output.with_extension("manifest.toml");
        if let Err(error) = manifest.write_atomic(&manifest_path) {
            let _ = std::fs::remove_file(&config.output);
            return Err(error.into());
        }
        Ok(ExportReport {
            frames: report.frames,
            bytes: report.bytes,
            output: report.output,
            manifest: manifest_path,
        })
    }

    fn render_into_sink<S: FrameSink>(
        params: RenderParams<'_>,
        session: &mut ReplaySession<H>,
        renderer: &mut SceneRenderer,
        readback: &mut GpuReadback,
        sink: &mut S,
    ) -> Result<(), ExportError> {
        let mut last_rendered_tick = None;
        for sample in params.schedule.samples() {
            let sample = sample?;
            if params.cancellation.load(Ordering::Acquire) {
                return Err(ExportError::Cancelled);
            }
            let snapshot = advance_session_to_tick(session, &mut last_rendered_tick, sample.tick)?;
            let view = derive_render_view(
                &snapshot,
                session.config().camera_mode,
                params.width,
                params.height,
            );
            renderer.set_view(view)?;
            let mut encoder =
                params
                    .context
                    .device()
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("gource-export-frame"),
                    });
            let view = params
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            renderer.render_to_target(
                &snapshot,
                session.catalog(),
                &mut encoder,
                gource_render::RenderTarget::new(
                    &view,
                    params.width,
                    params.height,
                    RENDER_TARGET_FORMAT,
                    1,
                ),
            )?;
            readback.encode_copy(&mut encoder, params.texture);
            params.context.queue().submit([encoder.finish()]);
            let pixels = readback.read_rgba8(params.context.device(), None)?;
            sink.push(sample.index, pixels)?;
        }
        Ok(())
    }

    fn ensure_context(&mut self) -> Result<(), ExportError> {
        if self.context.is_none() {
            self.context = Some(pollster::block_on(GpuContext::request_headless())?);
        }
        let context = self
            .context
            .as_ref()
            .ok_or(ExportError::InvalidSpec("GPU context unavailable"))?;
        validate_texture_extent_limit(
            context.diagnostics().limits.max_texture_dimension_2d,
            self.spec.width,
            self.spec.height,
        )
    }

    fn make_renderer(
        spec: &ExportSpec,
        context: &GpuContext,
    ) -> Result<SceneRenderer, ExportError> {
        let config = RendererConfig {
            view: RenderView {
                width: spec.width,
                height: spec.height,
                ..RenderView::default()
            },
            label_budget: spec.label_budget,
            clear_color: spec.clear_color,
            ..RendererConfig::default()
        };
        Ok(SceneRenderer::new(context, config)?)
    }

    fn backend_identity(&self) -> BackendIdentity {
        self.context
            .as_ref()
            .map(|context| backend_identity_from_info(&context.diagnostics().info))
            .unwrap_or_else(|| BackendIdentity {
                backend: "headless-requested".to_owned(),
                adapter: "unknown".to_owned(),
                driver: "unknown".to_owned(),
                driver_version: "unknown".to_owned(),
            })
    }
}

/// Convert adapter metadata into a manifest identity without requiring a live
/// graphics context.
fn backend_identity_from_info(info: &wgpu::AdapterInfo) -> BackendIdentity {
    BackendIdentity {
        backend: format!("{:?}", info.backend),
        adapter: info.name.clone(),
        driver: info.driver.clone(),
        driver_version: info.driver_info.clone(),
    }
}

fn validate_texture_extent_limit(maximum: u32, width: u32, height: u32) -> Result<(), ExportError> {
    if width > maximum || height > maximum {
        return Err(ExportError::TextureExtentLimit {
            width,
            height,
            maximum,
        });
    }
    Ok(())
}

const CAMERA_CONTENT_SCALE: f64 = 0.88;
const CAMERA_MIN_ZOOM: f64 = 1.0e-6;
const CAMERA_MAX_ZOOM: f64 = 100.0;
const CAMERA_MIN_HALF_EXTENT: f64 = 1.0;

/// Derive the complete presentation camera from one immutable replay snapshot.
///
/// The simulation owns the visual bounds, so this helper only chooses the
/// viewport framing.  In particular, no camera state is carried between
/// frames: a frame is a pure function of its snapshot, camera policy, and
/// output dimensions.
#[must_use]
fn derive_render_view(
    snapshot: &SceneSnapshot,
    mode: CameraMode,
    width: u32,
    height: u32,
) -> RenderView {
    let width = width.max(1);
    let height = height.max(1);
    let Some(bounds) = snapshot.bounds() else {
        return RenderView {
            width,
            height,
            ..RenderView::default()
        };
    };

    let min_x = finite_world(bounds.min.x);
    let min_y = finite_world(bounds.min.y);
    let max_x = finite_world(bounds.max.x);
    let max_y = finite_world(bounds.max.y);
    let (min_x, max_x) = if min_x <= max_x {
        (min_x, max_x)
    } else {
        (max_x, min_x)
    };
    let (min_y, max_y) = if min_y <= max_y {
        (min_y, max_y)
    } else {
        (max_y, min_y)
    };
    let overview_center = [midpoint(min_x, max_x), midpoint(min_y, max_y)];
    let center = match mode {
        CameraMode::Overview => overview_center,
        CameraMode::Track => track_center(snapshot).unwrap_or(overview_center),
    };
    let zoom = fit_zoom([min_x, min_y, max_x, max_y], center, width, height);
    RenderView {
        width,
        height,
        center: gource_render::RenderPoint::new(
            finite_render_coordinate(center[0]),
            finite_render_coordinate(center[1]),
        ),
        zoom,
        rotation_radians: 0.0,
    }
}

fn track_center(snapshot: &SceneSnapshot) -> Option<[f64; 2]> {
    let mut contributor_x = 0.0;
    let mut contributor_y = 0.0;
    let mut contributor_count = 0_u64;
    for contributor in &snapshot.contributors {
        if contributor.active {
            contributor_x += finite_world(contributor.position.x);
            contributor_y += finite_world(contributor.position.y);
            contributor_count = contributor_count.saturating_add(1);
        }
    }
    if contributor_count != 0 {
        let count = contributor_count as f64;
        return Some([contributor_x / count, contributor_y / count]);
    }

    let mut file_x = 0.0;
    let mut file_y = 0.0;
    let mut file_count = 0_u64;
    for file in &snapshot.files {
        if file.active {
            file_x += finite_world(file.position.x);
            file_y += finite_world(file.position.y);
            file_count = file_count.saturating_add(1);
        }
    }
    (file_count != 0).then(|| {
        let count = file_count as f64;
        [file_x / count, file_y / count]
    })
}

fn fit_zoom(bounds: [f64; 4], center: [f64; 2], width: u32, height: u32) -> f32 {
    let aspect = (width as f64 / height.max(1) as f64).max(f64::MIN_POSITIVE);
    let half_width = (bounds[2] - center[0])
        .abs()
        .max((bounds[0] - center[0]).abs())
        .max(CAMERA_MIN_HALF_EXTENT);
    let half_height = (bounds[3] - center[1])
        .abs()
        .max((bounds[1] - center[1]).abs())
        .max(CAMERA_MIN_HALF_EXTENT);
    let extent = half_height.max(half_width / aspect);
    let candidate = CAMERA_CONTENT_SCALE / extent;
    let zoom = if candidate.is_finite() && candidate > 0.0 {
        candidate.clamp(CAMERA_MIN_ZOOM, CAMERA_MAX_ZOOM)
    } else {
        1.0
    };
    zoom as f32
}

fn midpoint(min: f64, max: f64) -> f64 {
    let value = min + (max - min) * 0.5;
    if value.is_finite() { value } else { 0.0 }
}

fn finite_world(value: f32) -> f64 {
    if value.is_finite() {
        f64::from(value)
    } else {
        0.0
    }
}

fn finite_render_coordinate(value: f64) -> f32 {
    if !value.is_finite() {
        return 0.0;
    }
    value.clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32
}

fn advance_session_to_tick<H: HistorySource>(
    session: &mut ReplaySession<H>,
    last_rendered_tick: &mut Option<u64>,
    target_tick: u64,
) -> Result<Arc<gource_sim::SceneSnapshot>, ReplayError> {
    match *last_rendered_tick {
        None if target_tick == 0 => {}
        None => {
            session.seek_tick(target_tick)?;
        }
        Some(previous_tick) if target_tick >= previous_tick => {
            let delta = target_tick - previous_tick;
            if delta != 0 {
                session.advance_ticks(delta)?;
            }
        }
        Some(_) => {
            return Err(ReplayError::InvalidConfig(
                "export schedule ticks must be monotonic",
            ));
        }
    }
    *last_rendered_tick = Some(target_tick);
    Ok(session.snapshot())
}

fn make_offscreen_texture(context: &GpuContext, width: u32, height: u32) -> wgpu::Texture {
    context.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("gource-export-target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: RENDER_TARGET_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

impl EncoderIdentity {
    fn frames() -> Self {
        Self {
            kind: "frames".to_owned(),
            executable: None,
            codec: Some("png".to_owned()),
            container: Some("directory".to_owned()),
            pixel_format: "rgba".to_owned(),
            color_space: "gamma-space".to_owned(),
            color_primaries: "unspecified".to_owned(),
            color_transfer: "unspecified".to_owned(),
            color_range: "full".to_owned(),
        }
    }

    fn ffmpeg(config: &FfmpegConfig) -> Self {
        Self {
            kind: "ffmpeg".to_owned(),
            executable: Some(config.executable.to_string_lossy().into_owned()),
            codec: Some("ffv1".to_owned()),
            container: Some("matroska".to_owned()),
            pixel_format: "rgba".to_owned(),
            color_space: config.color_space.clone(),
            color_primaries: config.color_primaries.clone(),
            color_transfer: config.color_transfer.clone(),
            color_range: config.color_range.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gource_core::{ContributorId, DirId, FileId, History, PathId, Rgb8};
    use gource_render::RenderPoint;
    use gource_sim::{ContributorVisual, DirectoryVisual, FileVisual, LabelVisual, Vec2};
    use tempfile::tempdir;

    #[test]
    fn empty_range_publishes_an_empty_frame_directory_without_gpu() {
        let directory = tempdir().unwrap();
        let output = directory.path().join("frames");
        let spec = ExportSpec {
            width: 2,
            height: 1,
            output: ExportOutput::frames(&output),
            end: Rational::ZERO,
            ..ExportSpec::default()
        };
        let mut job = ExportJob::new(History::empty(), spec);
        let report = job.run().unwrap();
        assert_eq!(report.frames, 0);
        assert!(output.join("manifest.toml").is_file());
    }

    #[test]
    fn narrow_export_specs_use_padded_readback_capacity() {
        for (width, height) in [(1, 1), (800, 600)] {
            let spec = ExportSpec::new(
                width,
                height,
                FrameRate::integer(60).unwrap(),
                Rational::ZERO,
                Rational::ONE,
                ExportOutput::frames("frames"),
                ReplayConfig::default(),
            )
            .unwrap();
            let layout = ReadbackLayout::rgba8(width, height).unwrap();
            let expected = (layout.byte_len as u64)
                .checked_mul(spec.readback_slots as u64)
                .unwrap();
            assert_eq!(spec.max_readback_bytes, expected);
            assert!(spec.validate().is_ok());
        }
    }

    #[test]
    fn oversized_adapter_extent_is_rejected_before_texture_creation() {
        let error = validate_texture_extent_limit(4_096, 4_097, 1).unwrap_err();
        assert!(matches!(
            error,
            ExportError::TextureExtentLimit {
                width: 4_097,
                height: 1,
                maximum: 4_096,
            }
        ));
    }

    fn point(x: f32, y: f32) -> Vec2 {
        Vec2::new(x, y)
    }

    fn directory(id: u32, x: f32, y: f32, radius: f32) -> DirectoryVisual {
        DirectoryVisual {
            id: DirId::new(id).unwrap(),
            position: point(x, y),
            radius,
            opacity: 1.0,
            color: Rgb8::new(60, 90, 140),
        }
    }

    fn file(id: u32, x: f32, y: f32, radius: f32, active: bool) -> FileVisual {
        FileVisual {
            path_id: PathId::new(id).unwrap(),
            file_id: FileId::new(id).unwrap(),
            incarnation: 0,
            position: point(x, y),
            radius,
            opacity: 1.0,
            active,
            color: Rgb8::new(120, 180, 220),
        }
    }

    fn contributor(id: u32, x: f32, y: f32, active: bool) -> ContributorVisual {
        ContributorVisual {
            id: ContributorId::new(id).unwrap(),
            position: point(x, y),
            energy: if active { 1.0 } else { 0.0 },
            active,
            color: Rgb8::new(220, 180, 100),
        }
    }

    fn label(id: u32, x: f32, y: f32, text: &str) -> LabelVisual {
        LabelVisual {
            target: PathId::new(id).unwrap(),
            text: text.to_owned(),
            position: point(x, y),
            opacity_bits: 1.0_f32.to_bits(),
            priority: 1,
            visible: true,
        }
    }

    fn assert_bounds_fit(snapshot: &SceneSnapshot, view: RenderView) {
        let bounds = snapshot.bounds().unwrap();
        let xs = [bounds.min.x, bounds.max.x];
        let ys = [bounds.min.y, bounds.max.y];
        for x in xs {
            for y in ys {
                let ndc = [
                    (x - view.center.x) * view.zoom / view.aspect(),
                    (y - view.center.y) * view.zoom,
                ];
                assert!(
                    ndc[0].abs() <= CAMERA_CONTENT_SCALE as f32 + 1.0e-4,
                    "x={ndc:?} view={view:?}"
                );
                assert!(
                    ndc[1].abs() <= CAMERA_CONTENT_SCALE as f32 + 1.0e-4,
                    "y={ndc:?} view={view:?}"
                );
            }
        }
    }

    #[test]
    fn overview_frames_translated_wide_and_portrait_snapshots() {
        let mut wide = SceneSnapshot::empty();
        wide.directories.push(directory(1, 1_000.0, -250.0, 4.0));
        wide.files.push(file(2, 1_120.0, -230.0, 3.0, true));
        wide.labels
            .push(label(2, 1_120.0, -230.0, "src/feature.rs"));
        let wide_view = derive_render_view(&wide, CameraMode::Overview, 1_920, 480);
        assert_bounds_fit(&wide, wide_view);

        let mut portrait = SceneSnapshot::empty();
        portrait.directories.push(directory(3, -700.0, 200.0, 2.0));
        portrait.files.push(file(4, -690.0, 238.0, 2.0, true));
        portrait.labels.push(label(4, -690.0, 238.0, "main.rs"));
        let portrait_view = derive_render_view(&portrait, CameraMode::Overview, 480, 1_920);
        assert_bounds_fit(&portrait, portrait_view);
    }

    #[test]
    fn empty_and_single_point_snapshots_have_safe_finite_views() {
        let empty = derive_render_view(&SceneSnapshot::empty(), CameraMode::Overview, 0, 0);
        assert_eq!(empty.width, 1);
        assert_eq!(empty.height, 1);
        assert_eq!(empty.center, RenderPoint::new(0.0, 0.0));
        assert_eq!(empty.zoom, 1.0);
        assert!(empty.validate().is_ok());

        let mut single = SceneSnapshot::empty();
        single
            .directories
            .push(directory(5, 12_345.0, -6_789.0, 0.0));
        let view = derive_render_view(&single, CameraMode::Overview, 320, 200);
        assert_eq!(view.center, RenderPoint::new(12_345.0, -6_789.0));
        assert!(view.zoom.is_finite() && view.zoom > 0.0);
        assert_bounds_fit(&single, view);
    }

    #[test]
    fn track_prefers_mean_active_contributors_then_active_files() {
        let mut snapshot = SceneSnapshot::empty();
        snapshot.contributors.extend([
            contributor(1, 10.0, 20.0, true),
            contributor(2, 30.0, 40.0, true),
        ]);
        snapshot.files.push(file(3, 100.0, 100.0, 2.0, true));
        let view = derive_render_view(&snapshot, CameraMode::Track, 800, 600);
        assert_eq!(view.center, RenderPoint::new(20.0, 30.0));
        assert_bounds_fit(&snapshot, view);

        let mut files_only = SceneSnapshot::empty();
        files_only
            .contributors
            .push(contributor(4, -100.0, -100.0, false));
        files_only.files.extend([
            file(5, 8.0, 12.0, 1.0, true),
            file(6, 20.0, 18.0, 1.0, true),
        ]);
        let file_view = derive_render_view(&files_only, CameraMode::Track, 800, 600);
        assert_eq!(file_view.center, RenderPoint::new(14.0, 15.0));
        assert_bounds_fit(&files_only, file_view);
    }

    #[test]
    fn track_without_active_target_falls_back_to_overview() {
        let mut snapshot = SceneSnapshot::empty();
        snapshot.directories.push(directory(9, -16.0, 24.0, 2.0));
        snapshot.files.push(file(10, 8.0, 12.0, 1.0, false));
        snapshot
            .contributors
            .push(contributor(11, 64.0, -32.0, false));
        let overview = derive_render_view(&snapshot, CameraMode::Overview, 640, 360);
        let track = derive_render_view(&snapshot, CameraMode::Track, 640, 360);
        assert_eq!(track, overview);
    }

    #[test]
    fn camera_derivation_is_repeatable_for_one_snapshot_and_resolution() {
        let mut snapshot = SceneSnapshot::empty();
        snapshot.directories.push(directory(7, 4.0, 5.0, 0.5));
        snapshot.files.push(file(8, 9.0, 7.0, 0.75, true));
        snapshot
            .labels
            .push(label(8, 9.0, 7.0, "unicode/данные.rs"));
        let first = derive_render_view(&snapshot, CameraMode::Overview, 1_111, 777);
        let second = derive_render_view(&snapshot, CameraMode::Overview, 1_111, 777);
        assert_eq!(first, second);
    }

    #[derive(Clone)]
    struct CountingHistory {
        inner: History,
        accesses: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl gource_core::HistorySource for CountingHistory {
        fn catalog(&self) -> &gource_core::Catalog {
            self.inner.catalog()
        }

        fn events(&self) -> &[gource_core::Event] {
            self.accesses.fetch_add(1, Ordering::Relaxed);
            self.inner.events()
        }
    }

    #[test]
    fn replay_samples_advance_linearly_from_the_initial_seek() {
        let accesses = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let history = CountingHistory {
            inner: History::empty(),
            accesses: Arc::clone(&accesses),
        };
        let config = ReplayConfig::default();
        let mut session = ReplaySession::new(history, config.clone()).unwrap();
        let mut last_rendered_tick = None;
        for target_tick in [120, 240, 360] {
            let snapshot =
                advance_session_to_tick(&mut session, &mut last_rendered_tick, target_tick)
                    .unwrap();
            assert_eq!(snapshot.tick, target_tick);
        }
        let expected = {
            let mut expected = ReplaySession::new(History::empty(), config).unwrap();
            expected.seek_tick(360).unwrap();
            expected.snapshot()
        };
        assert_eq!(*session.snapshot(), *expected);
        assert!(
            accesses.load(Ordering::Relaxed) < 500,
            "replay history was rescanned for every frame"
        );
    }

    #[test]
    fn manifest_backend_identity_preserves_adapter_metadata() {
        let mut info =
            wgpu::AdapterInfo::new(wgpu::DeviceType::IntegratedGpu, wgpu::Backend::Vulkan);
        info.name = "test adapter".to_owned();
        info.driver = "test driver".to_owned();
        info.driver_info = "test driver version".to_owned();
        let spec = ExportSpec::default();
        let schedule = ExportSchedule::new(
            Rational::ZERO,
            Rational::new(1, 120).unwrap(),
            spec.frame_rate,
        )
        .unwrap();
        let history = History::empty();
        let manifest = ExportManifestV1::from_history(
            &history,
            spec.width,
            spec.height,
            &schedule,
            &spec.replay,
            backend_identity_from_info(&info),
            EncoderIdentity::frames(),
        );
        assert_eq!(
            manifest.backend,
            BackendIdentity {
                backend: "Vulkan".to_owned(),
                adapter: "test adapter".to_owned(),
                driver: "test driver".to_owned(),
                driver_version: "test driver version".to_owned(),
            }
        );
    }
}
