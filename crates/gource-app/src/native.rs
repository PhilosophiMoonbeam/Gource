// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native winit application and the single-device wgpu presenter.

use egui_wgpu::{
    Renderer as EguiRenderer, RendererOptions as EguiRendererOptions, ScreenDescriptor,
};
use gource_core::{Catalog, Event, HistorySource, PathId, PlaybackClock, Rational, RationalError};
use gource_export::{
    ExportJob, ExportOutput, ExportSpec, FfmpegConfig, FrameRate as ExportFrameRate,
};
use gource_ingest::{
    CacheConfig, CacheKey, EventCache, IndexedHistory, InputSpec, input_identity_path, parse_input,
};
use gource_render::{
    GpuContext, RENDER_TARGET_FORMAT, RenderTarget, RenderView, RendererConfig, SceneRenderer,
};
use gource_sim::{RationalTime, ReplayError, ReplaySession, SIMULATION_HZ, SceneSnapshot};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowAttributes, WindowId};

use crate::config::{AppConfig, CommandKind, ConfigError};
use crate::input::{AppCommand, CameraControlMode, InputState};

/// A filtered immutable source retaining the original catalog and canonical
/// event order.  Filtering is done after ingest has validated and sorted the
/// finite source, so it cannot create a partially indexed history.
#[derive(Clone, Debug)]
pub struct FilteredHistory {
    base: IndexedHistory,
    events: Vec<Event>,
}

impl FilteredHistory {
    pub fn new(base: IndexedHistory, filters: &[String]) -> Self {
        let events = if filters.is_empty() {
            base.events().to_vec()
        } else {
            base.events()
                .iter()
                .filter(|event| {
                    base.catalog().path(event.path_id()).is_some_and(|path| {
                        filters
                            .iter()
                            .any(|filter| glob_match(filter, path.canonical()))
                    })
                })
                .cloned()
                .collect()
        };
        Self { base, events }
    }

    pub fn base(&self) -> &IndexedHistory {
        &self.base
    }

    pub fn catalog(&self) -> &Catalog {
        self.base.catalog()
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

impl HistorySource for FilteredHistory {
    fn catalog(&self) -> &Catalog {
        self.catalog()
    }

    fn events(&self) -> &[Event] {
        self.events()
    }
}

/// Parse and index one finite input for a configured command.
pub fn load_history(config: &AppConfig) -> Result<FilteredHistory, NativeError> {
    let options = config.ingest_options();
    let history = match (&config.cache_dir, &config.input) {
        (Some(directory), InputSpec::File(path)) if path != Path::new("-") && path.is_file() => {
            let identity = input_identity_path(path, &options)?;
            let key = CacheKey::new(identity, &options);
            let cache = EventCache::open(
                CacheConfig::new(directory.clone())
                    .with_max_bytes(config.cache_bytes)
                    .with_max_entry_bytes(config.cache_bytes),
            )?;
            if let Some(history) = cache.load(&key)? {
                history
            } else {
                let history = parse_input(config.input.clone(), options.clone())?;
                cache.store(&key, &history)?;
                history
            }
        }
        _ => parse_input(config.input.clone(), options)?,
    };
    Ok(FilteredHistory::new(history, &config.filters))
}

/// Run the interactive `view` command using a real winit event loop.
pub fn run_view(config: AppConfig) -> Result<(), NativeError> {
    if config.command != CommandKind::View {
        return Err(NativeError::WrongCommand("view"));
    }
    config.validate()?;
    let history = load_history(&config)?;
    let event_loop = EventLoop::new().map_err(|error| NativeError::EventLoop(error.to_string()))?;
    let mut app = NativeApplication::new(config, history)?;
    event_loop
        .run_app(&mut app)
        .map_err(|error| NativeError::EventLoop(error.to_string()))?;
    app.take_error().map_or(Ok(()), Err)
}
/// Run a headless `export` command through gource-export's public runner.
/// There is intentionally no second scheduler or renderer in this crate.
pub fn run_export(config: AppConfig) -> Result<gource_export::ExportReport, NativeError> {
    if config.command != CommandKind::Export {
        return Err(NativeError::WrongCommand("export"));
    }
    config.validate()?;
    let history = load_history(&config)?;
    let frame_rate = ExportFrameRate::new(
        u64::from(config.frame_rate.numerator),
        u64::from(config.frame_rate.denominator),
    )
    .map_err(|error| NativeError::Export(error.to_string()))?;
    let (start, end) = playback_export_range(&config, &history)?;
    let output_path = config
        .output
        .clone()
        .ok_or(ConfigError::MissingExportOutput)?;
    let output = if config.video {
        ExportOutput::video(FfmpegConfig::new(
            output_path,
            config.viewport.width,
            config.viewport.height,
            frame_rate,
        ))
    } else {
        ExportOutput::frames(output_path)
    };
    let spec = ExportSpec::new(
        config.viewport.width,
        config.viewport.height,
        frame_rate,
        start,
        end,
        output,
        config.replay.clone(),
    )
    .map_err(|error| NativeError::Export(error.to_string()))?;
    let mut job = ExportJob::new(history, spec);
    job.run()
        .map_err(|error| NativeError::Export(error.to_string()))
}

fn validated_repository_rate(
    config: &AppConfig,
    start_timestamp: i64,
) -> Result<Rational, ConfigError> {
    PlaybackClock::from_config(&config.replay, start_timestamp)
        .map(|clock| clock.repository_rate)
        .map_err(ConfigError::ReplayConfig)
}

fn playback_relative_time(
    repository_time: Rational,
    origin_timestamp: i64,
    repository_rate: Rational,
) -> Result<Rational, RationalError> {
    let origin = Rational::new(origin_timestamp as i128, 1)?;
    let elapsed_numerator = repository_time
        .numerator
        .checked_mul(origin.denominator)
        .and_then(|left| {
            origin
                .numerator
                .checked_mul(repository_time.denominator)
                .and_then(|right| left.checked_sub(right))
        })
        .ok_or(RationalError::Overflow)?;
    let elapsed_denominator = repository_time
        .denominator
        .checked_mul(origin.denominator)
        .ok_or(RationalError::Overflow)?;
    Rational::new(
        elapsed_numerator
            .checked_mul(repository_rate.denominator)
            .ok_or(RationalError::Overflow)?,
        elapsed_denominator
            .checked_mul(repository_rate.numerator)
            .ok_or(RationalError::Overflow)?,
    )
}

fn playback_timestamp_tick(
    timestamp: i64,
    origin_timestamp: i64,
    repository_rate: Rational,
) -> Result<u64, RationalError> {
    let wall_time = playback_relative_time(
        Rational::new(timestamp as i128, 1)?,
        origin_timestamp,
        repository_rate,
    )?;
    if wall_time.numerator <= 0 {
        return Ok(0);
    }
    let ticks = wall_time.checked_mul(Rational::from_u64(SIMULATION_HZ))?;
    let whole_ticks = ticks
        .numerator
        .checked_div(ticks.denominator)
        .ok_or(RationalError::Overflow)?;
    let remainder = ticks
        .numerator
        .checked_rem(ticks.denominator)
        .ok_or(RationalError::Overflow)?;
    let tick = whole_ticks
        .checked_add(if remainder == 0 { 0 } else { 1 })
        .ok_or(RationalError::Overflow)?;
    u64::try_from(tick).map_err(|_| RationalError::Overflow)
}

/// Convert a finite signed duration into the exact decimal rational accepted
/// by the replay clock.  The input is a UI command, so parsing its spelling
/// through `Rational::from_f64` keeps seek arithmetic out of binary-float
/// multiplication and truncation.
fn signed_repository_seconds(seconds: f64) -> Result<Rational, RationalError> {
    if !seconds.is_finite() {
        return Err(RationalError::NonFinite);
    }
    let magnitude = Rational::from_f64(seconds.abs())?;
    if seconds.is_sign_negative() && magnitude.numerator != 0 {
        Rational::new(
            magnitude
                .numerator
                .checked_neg()
                .ok_or(RationalError::Overflow)?,
            magnitude.denominator,
        )
    } else {
        Ok(magnitude)
    }
}

fn repository_time_after_seconds(
    current: RationalTime,
    seconds: f64,
) -> Result<RationalTime, RationalError> {
    if !seconds.is_finite() {
        return Ok(current);
    }
    let current = Rational::new(current.numerator, current.denominator)?;
    let target = current.checked_add(signed_repository_seconds(seconds)?)?;
    Ok(RationalTime::from(target))
}

/// Compare an integer event timestamp to a rational replay position without
/// converting either side through `f64` or overflowing a cross multiplication.
fn event_timestamp_is_after(timestamp: i64, current: RationalTime) -> bool {
    if current.denominator <= 0 {
        return false;
    }
    let floor = current.numerator.div_euclid(current.denominator);
    i128::from(timestamp) > floor
}

fn playback_export_range(
    config: &AppConfig,
    history: &FilteredHistory,
) -> Result<(Rational, Rational), NativeError> {
    let first_timestamp = history.events().first().map(Event::timestamp).unwrap_or(0);
    let repository_rate = validated_repository_rate(config, first_timestamp)?;
    let start_repository = if config.start.0 == Rational::ZERO {
        Rational::new(first_timestamp as i128, 1).unwrap_or(Rational::ZERO)
    } else {
        config.start.0
    };
    let end_repository = config.end.map(|time| time.0).unwrap_or_else(|| {
        history
            .events()
            .last()
            .and_then(|event| Rational::new(event.timestamp() as i128 + 1, 1).ok())
            .unwrap_or(start_repository)
    });
    let start = playback_relative_time(start_repository, first_timestamp, repository_rate)
        .map_err(|error| NativeError::Export(error.to_string()))?;
    let end = playback_relative_time(end_repository, first_timestamp, repository_rate)
        .map_err(|error| NativeError::Export(error.to_string()))?;
    Ok((start, end))
}

/// Native application state.  The window and presentation objects are created
/// from `resumed`, as required by winit 0.30; no graphics object is created in
/// a background thread.
pub struct NativeApplication {
    config: AppConfig,
    replay: ReplaySession<FilteredHistory>,
    repository_origin: i64,
    repository_rate: Rational,
    input: InputState,
    selected_path: Option<PathId>,
    window: Option<Arc<Window>>,
    graphics: Option<GraphicsState>,
    last_frame: Instant,
    occluded: bool,
    fatal_error: Option<NativeError>,
    redraw_requested: bool,
    next_repaint_at: Option<Instant>,
    surface_retry_at: Option<Instant>,
}
impl NativeApplication {
    pub fn new(config: AppConfig, history: FilteredHistory) -> Result<Self, NativeError> {
        config.validate()?;
        let repository_origin = history.events().first().map(Event::timestamp).unwrap_or(0);
        let repository_rate = validated_repository_rate(&config, repository_origin)?;
        let replay = ReplaySession::new(history.clone(), config.replay.clone())?;
        let mut input = InputState::default();
        input.set_viewport(config.viewport.width, config.viewport.height);
        input.set_camera_mode(config.replay.camera_mode.into());
        Ok(Self {
            config,
            replay,
            repository_origin,
            repository_rate,
            input,
            selected_path: None,
            window: None,
            graphics: None,
            last_frame: Instant::now(),
            occluded: false,
            fatal_error: None,
            redraw_requested: true,
            surface_retry_at: None,
            next_repaint_at: None,
        })
    }

    pub fn input(&self) -> &InputState {
        &self.input
    }

    pub fn input_mut(&mut self) -> &mut InputState {
        &mut self.input
    }

    pub fn replay(&self) -> &ReplaySession<FilteredHistory> {
        &self.replay
    }

    pub fn window(&self) -> Option<&Arc<Window>> {
        self.window.as_ref()
    }

    pub fn take_error(&mut self) -> Option<NativeError> {
        self.fatal_error.take()
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: NativeError) {
        tracing::error!(error = %error, "native application stopped");
        if self.fatal_error.is_none() {
            self.fatal_error = Some(error);
        }
        self.shutdown();
        event_loop.exit();
    }

    fn shutdown(&mut self) {
        self.redraw_requested = false;
        self.next_repaint_at = None;
        self.surface_retry_at = None;
        // Drop graphics before the window.  GraphicsState owns the surface,
        // which in turn retains an Arc to the native window.
        self.graphics.take();
        self.window.take();
    }

    fn can_render(&self) -> bool {
        self.input.camera.viewport[0] > 0
            && self.input.camera.viewport[1] > 0
            && self
                .graphics
                .as_ref()
                .is_some_and(GraphicsState::is_renderable)
    }
    fn reset_camera(&mut self) {
        self.input.camera.reset();
        self.input.right_dragging = false;
        self.input.left_selecting = false;
        self.redraw_requested = true;
    }

    fn handle_commands(&mut self, event_loop: &ActiveEventLoop, commands: Vec<AppCommand>) {
        let has_commands = !commands.is_empty();
        for command in commands {
            match command {
                AppCommand::ResetCamera => self.reset_camera(),
                AppCommand::TogglePause => self.input.paused = !self.input.paused,
                AppCommand::SetPaused(paused) => self.input.paused = paused,
                AppCommand::AdjustPlaybackRate(multiplier) => {
                    if multiplier.is_finite() && multiplier > 0.0 {
                        self.input.playback_rate =
                            (self.input.playback_rate * multiplier).clamp(0.05, 64.0);
                    }
                }
                AppCommand::SetPlaybackRate(rate) => {
                    if rate.is_finite() && rate > 0.0 {
                        self.input.playback_rate = rate.clamp(0.05, 64.0);
                    }
                }
                AppCommand::SeekTick(tick) => {
                    if let Err(error) = self.replay.seek_tick(tick) {
                        self.fail(event_loop, NativeError::Replay(error));
                        return;
                    }
                }
                AppCommand::SeekRepositorySeconds(seconds) => {
                    let current = self.replay.repository_time();
                    let target = match repository_time_after_seconds(current, seconds) {
                        Ok(target) => target,
                        Err(error) => {
                            self.fail(event_loop, NativeError::Export(error.to_string()));
                            return;
                        }
                    };
                    if let Err(error) = self.replay.seek_repository_time(target) {
                        self.fail(event_loop, NativeError::Replay(error));
                        return;
                    }
                }
                AppCommand::NextEvent => {
                    let current = self.replay.repository_time();
                    if let Some(event) = self
                        .replay
                        .history()
                        .events()
                        .iter()
                        .find(|event| event_timestamp_is_after(event.timestamp(), current))
                    {
                        let tick = match playback_timestamp_tick(
                            event.timestamp(),
                            self.repository_origin,
                            self.repository_rate,
                        ) {
                            Ok(tick) => tick,
                            Err(error) => {
                                self.fail(event_loop, NativeError::Export(error.to_string()));
                                return;
                            }
                        };
                        if let Err(error) = self.replay.seek_tick(tick) {
                            self.fail(event_loop, NativeError::Replay(error));
                            return;
                        }
                    }
                }
                AppCommand::Zoom(factor) => {
                    if factor.is_finite() && factor > 0.0 {
                        let zoom =
                            if self.input.camera.zoom.is_finite() && self.input.camera.zoom > 0.0 {
                                self.input.camera.zoom
                            } else {
                                1.0
                            };
                        self.input.camera.zoom = (zoom * factor).clamp(0.02, 100.0);
                        self.input.camera.mode = CameraControlMode::Manual;
                    }
                }
                AppCommand::SetCameraMode(mode) => self.input.set_camera_mode(mode),
                AppCommand::SelectAt { x, y } => {
                    let snapshot = self.replay.snapshot();
                    self.selected_path = self.pick_file(&snapshot, x, y);
                    self.input.camera.mode = CameraControlMode::Track;
                }
                AppCommand::ClearSelection => self.selected_path = None,
                AppCommand::Quit => {
                    self.shutdown();
                    event_loop.exit();
                    return;
                }
            }
        }
        if should_request_redraw(has_commands, false) {
            self.redraw_requested = true;
        }
    }

    fn pick_file(&self, snapshot: &SceneSnapshot, x: f64, y: f64) -> Option<PathId> {
        let view = self.input.camera.view();
        if view.width == 0
            || view.height == 0
            || !x.is_finite()
            || !y.is_finite()
            || x < 0.0
            || y < 0.0
            || x > f64::from(view.width)
            || y > f64::from(view.height)
            || !view.zoom.is_finite()
            || view.zoom <= 0.0
        {
            return None;
        }
        let aspect = view.aspect();
        let nx = x as f32 / view.width as f32 * 2.0 - 1.0;
        let ny = 1.0 - y as f32 / view.height as f32 * 2.0;
        let world_x = view.center.x + nx * aspect / view.zoom;
        let world_y = view.center.y + ny / view.zoom;
        let radius = 0.16 / view.zoom;
        snapshot
            .files
            .iter()
            .filter_map(|file| {
                let dx = file.position.x - world_x;
                let dy = file.position.y - world_y;
                let distance = dx * dx + dy * dy;
                (distance <= radius * radius).then_some((distance, file.path_id))
            })
            .min_by(|left, right| left.0.total_cmp(&right.0))
            .map(|(_, path)| path)
    }

    fn update_camera(&mut self, snapshot: &SceneSnapshot) {
        match self.input.camera.mode {
            CameraControlMode::Overview => {
                let mut bounds = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
                let mut found = false;
                for point in snapshot
                    .directories
                    .iter()
                    .map(|visual| visual.position)
                    .chain(snapshot.files.iter().map(|visual| visual.position))
                    .chain(snapshot.contributors.iter().map(|visual| visual.position))
                {
                    found = true;
                    bounds[0] = bounds[0].min(point.x);
                    bounds[1] = bounds[1].min(point.y);
                    bounds[2] = bounds[2].max(point.x);
                    bounds[3] = bounds[3].max(point.y);
                }
                if found {
                    let width = (bounds[2] - bounds[0]).max(1.0);
                    let height = (bounds[3] - bounds[1]).max(1.0);
                    let aspect = self.input.camera.viewport[0].max(1) as f32
                        / self.input.camera.viewport[1].max(1) as f32;
                    self.input.camera.center =
                        [(bounds[0] + bounds[2]) * 0.5, (bounds[1] + bounds[3]) * 0.5];
                    self.input.camera.zoom =
                        (1.0 / (width / aspect).max(height) * 0.88).clamp(0.02, 100.0);
                }
            }
            CameraControlMode::Track => {
                if let Some(path) = self.selected_path
                    && let Some(file) = snapshot.files.iter().find(|file| file.path_id == path)
                {
                    self.input.camera.center = [file.position.x, file.position.y];
                }
            }
            CameraControlMode::Manual => {}
        }
    }

    fn timeline_total_ticks(&self) -> u64 {
        self.replay
            .history()
            .events()
            .last()
            .and_then(|event| {
                playback_timestamp_tick(
                    event.timestamp(),
                    self.repository_origin,
                    self.repository_rate,
                )
                .ok()
            })
            .unwrap_or(1)
            .max(1)
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let now = Instant::now();
        if self.occluded || !self.can_render() {
            self.redraw_requested = false;
            self.next_repaint_at = None;
            self.last_frame = now;
            return;
        }
        if self.surface_retry_at.is_some_and(|deadline| deadline > now) {
            return;
        }
        self.surface_retry_at = None;
        self.redraw_requested = false;
        let elapsed = now.saturating_duration_since(self.last_frame);
        self.last_frame = now;
        if self.input.focused && !self.input.paused {
            let elapsed = scale_duration(elapsed, self.input.playback_rate);
            if let Err(error) = self.replay.advance_wall(elapsed) {
                self.fail(event_loop, NativeError::Replay(error));
                return;
            }
        }
        let snapshot = self.replay.snapshot();
        self.update_camera(&snapshot);
        let total_ticks = self.timeline_total_ticks();
        let selected_label = self
            .selected_path
            .and_then(|path| self.replay.catalog().path(path))
            .map(|path| path.canonical().to_owned());
        let ui_data = UiData {
            paused: self.input.paused,
            playback_rate: self.input.playback_rate,
            tick: snapshot.tick,
            total_ticks,
            selected_label,
            filter_count: self.config.filters.len(),
        };
        let camera = self.input.camera.view();
        let result = self.graphics.as_mut().map(|graphics| {
            graphics.render(&window, &snapshot, self.replay.catalog(), camera, ui_data)
        });
        match result {
            Some(Ok((commands, outcome))) => {
                let has_commands = !commands.is_empty();
                self.handle_commands(event_loop, commands);
                if self.fatal_error.is_some() || self.window.is_none() {
                    return;
                }
                match outcome {
                    RenderOutcome::Presented { repaint_delay } => {
                        self.surface_retry_at = None;
                        self.redraw_requested |=
                            should_request_redraw(has_commands, repaint_delay == Duration::ZERO);
                        self.next_repaint_at = repaint_deadline(Instant::now(), repaint_delay);
                    }
                    RenderOutcome::Retry { delay } => {
                        self.redraw_requested |= has_commands;
                        self.next_repaint_at = None;
                        self.surface_retry_at = Instant::now().checked_add(delay);
                    }
                    RenderOutcome::Occluded => {
                        self.occluded = true;
                        self.redraw_requested |= has_commands;
                        self.next_repaint_at = None;
                        self.surface_retry_at = None;
                    }
                }
            }
            Some(Err(error)) => self.fail(event_loop, error),
            None => {}
        }
    }
}

impl ApplicationHandler for NativeApplication {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(window) = self.window.clone() {
            if self.graphics.is_none() {
                match GraphicsState::new(&window, self.config.viewport) {
                    Ok(graphics) => self.graphics = Some(graphics),
                    Err(error) => {
                        self.fail(event_loop, error);
                        return;
                    }
                }
            }
            self.redraw_requested = true;
            window.request_redraw();
            return;
        }
        let attributes: WindowAttributes = Window::default_attributes()
            .with_title("Gource")
            .with_inner_size(PhysicalSize::new(
                self.config.viewport.width,
                self.config.viewport.height,
            ));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                self.fail(event_loop, NativeError::Window(error.to_string()));
                return;
            }
        };
        self.input.focused = true;
        self.input
            .set_viewport(self.config.viewport.width, self.config.viewport.height);
        match GraphicsState::new(&window, self.config.viewport) {
            Ok(graphics) => {
                self.graphics = Some(graphics);
                self.window = Some(Arc::clone(&window));
                self.occluded = false;
                self.last_frame = Instant::now();
                self.redraw_requested = true;
                self.next_repaint_at = None;
                self.surface_retry_at = None;
                window.request_redraw();
            }
            Err(error) => self.fail(event_loop, error),
        }
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        self.shutdown();
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.shutdown();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.clone() else {
            return;
        };
        if window.id() != window_id {
            return;
        }
        let egui_response = self
            .graphics
            .as_mut()
            .map(|graphics| graphics.egui_state.on_window_event(&window, &event))
            .unwrap_or_default();
        let consumed = egui_response.consumed;
        let egui_repaint = egui_response.repaint;
        let camera_before = self.input.camera;
        let commands = self.input.on_window_event(&event, consumed);
        self.handle_commands(event_loop, commands);
        if self.fatal_error.is_some() || self.window.is_none() {
            return;
        }
        if egui_repaint {
            self.redraw_requested = true;
        }
        match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                self.shutdown();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(graphics) = self.graphics.as_mut()
                    && let Err(error) = graphics.resize(size)
                {
                    self.fail(event_loop, error);
                    return;
                }
                self.input.set_viewport(size.width, size.height);
                self.next_repaint_at = None;
                self.surface_retry_at = None;
                self.redraw_requested = true;
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                // winit reports the resulting physical extent through the
                // window after the scale-factor transition.  A matching
                // Resized event, when emitted by the platform, is harmless.
                let size = window.inner_size();
                if let Some(graphics) = self.graphics.as_mut()
                    && let Err(error) = graphics.resize(size)
                {
                    self.fail(event_loop, error);
                    return;
                }
                self.input.set_viewport(size.width, size.height);
                self.next_repaint_at = None;
                self.surface_retry_at = None;
                self.redraw_requested = true;
            }
            WindowEvent::Occluded(occluded) => {
                self.occluded = occluded;
                self.next_repaint_at = None;
                self.surface_retry_at = None;
                self.redraw_requested = !occluded;
            }
            WindowEvent::Focused(focused) => {
                self.input.focused = focused;
                if focused {
                    self.redraw_requested = true;
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {
                if self.input.camera != camera_before {
                    self.redraw_requested = true;
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(window) = self.window.as_ref() else {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        };
        let now = Instant::now();
        if self
            .surface_retry_at
            .is_some_and(|deadline| deadline <= now)
        {
            self.surface_retry_at = None;
            self.redraw_requested = true;
        }
        if self.next_repaint_at.is_some_and(|deadline| deadline <= now) {
            self.next_repaint_at = None;
            self.redraw_requested = true;
        }
        let retry_pending = self.surface_retry_at.is_some();
        let renderable = self.can_render();
        let active = renderable
            && !self.occluded
            && self.input.focused
            && !self.input.paused
            && !retry_pending;
        let deadline = self.surface_retry_at.or(self.next_repaint_at);
        event_loop.set_control_flow(if active {
            ControlFlow::Poll
        } else if let Some(deadline) = deadline {
            ControlFlow::WaitUntil(deadline)
        } else {
            ControlFlow::Wait
        });
        if renderable && !self.occluded && !retry_pending && (active || self.redraw_requested) {
            window.request_redraw();
        }
    }
}

/// Data displayed by the transport/timeline/settings UI.
#[derive(Clone, Debug)]
struct UiData {
    paused: bool,
    playback_rate: f64,
    tick: u64,
    total_ticks: u64,
    selected_label: Option<String>,
    filter_count: usize,
}

/// Result of trying to acquire and present one frame.
///
/// Transient surface states are deliberately represented separately from a
/// successful frame so the event loop can wait before retrying instead of
/// requesting redraws in a tight loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenderOutcome {
    Presented { repaint_delay: Duration },
    Retry { delay: Duration },
    Occluded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResizePolicy {
    configure_surface: bool,
    release_scene_texture: bool,
    release_scene_bind_group: bool,
}

fn resize_policy(size: PhysicalSize<u32>) -> ResizePolicy {
    let zero_extent = size.width == 0 || size.height == 0;
    ResizePolicy {
        configure_surface: !zero_extent,
        release_scene_texture: zero_extent,
        release_scene_bind_group: zero_extent,
    }
}

const SURFACE_RETRY_DELAY: Duration = Duration::from_millis(16);

fn choose_surface_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    [
        wgpu::TextureFormat::Bgra8UnormSrgb,
        wgpu::TextureFormat::Rgba8UnormSrgb,
        wgpu::TextureFormat::Bgra8Unorm,
        wgpu::TextureFormat::Rgba8Unorm,
    ]
    .into_iter()
    .find(|preferred| formats.contains(preferred))
    .or_else(|| formats.first().copied())
}

fn choose_present_mode(modes: &[wgpu::PresentMode]) -> Option<wgpu::PresentMode> {
    modes
        .contains(&wgpu::PresentMode::Fifo)
        .then_some(wgpu::PresentMode::Fifo)
        .or_else(|| modes.first().copied())
}

fn choose_alpha_mode(modes: &[wgpu::CompositeAlphaMode]) -> Option<wgpu::CompositeAlphaMode> {
    modes
        .contains(&wgpu::CompositeAlphaMode::Opaque)
        .then_some(wgpu::CompositeAlphaMode::Opaque)
        .or_else(|| {
            modes
                .contains(&wgpu::CompositeAlphaMode::Auto)
                .then_some(wgpu::CompositeAlphaMode::Auto)
        })
        .or_else(|| modes.first().copied())
}

fn validate_surface_extent(
    adapter: &wgpu::Adapter,
    width: u32,
    height: u32,
) -> Result<(), NativeError> {
    let maximum = adapter.limits().max_texture_dimension_2d;
    if width > maximum || height > maximum {
        return Err(NativeError::Surface(format!(
            "surface extent {width}x{height} exceeds adapter limit {maximum}"
        )));
    }
    Ok(())
}

fn validate_surface_configuration(
    capabilities: &wgpu::SurfaceCapabilities,
    configuration: &wgpu::SurfaceConfiguration,
) -> Result<(), NativeError> {
    if configuration.width == 0 || configuration.height == 0 {
        return Err(NativeError::Surface(
            "zero-sized viewport cannot configure surface".to_owned(),
        ));
    }
    if !capabilities.usages.contains(configuration.usage) {
        return Err(NativeError::Surface(
            "surface does not support render-attachment usage".to_owned(),
        ));
    }
    if !capabilities.formats.contains(&configuration.format) {
        return Err(NativeError::Surface(format!(
            "surface format {:?} is no longer supported",
            configuration.format
        )));
    }
    if !capabilities
        .present_modes
        .contains(&configuration.present_mode)
    {
        return Err(NativeError::Surface(format!(
            "surface present mode {:?} is no longer supported",
            configuration.present_mode
        )));
    }
    if !capabilities.alpha_modes.contains(&configuration.alpha_mode) {
        return Err(NativeError::Surface(format!(
            "surface alpha mode {:?} is no longer supported",
            configuration.alpha_mode
        )));
    }
    Ok(())
}

fn surface_configuration(
    surface: &wgpu::Surface<'_>,
    adapter: &wgpu::Adapter,
    width: u32,
    height: u32,
) -> Result<wgpu::SurfaceConfiguration, NativeError> {
    if width == 0 || height == 0 {
        return Err(NativeError::Surface(
            "zero-sized viewport cannot configure surface".to_owned(),
        ));
    }
    validate_surface_extent(adapter, width, height)?;
    let capabilities = surface.get_capabilities(adapter);
    let format = choose_surface_format(&capabilities.formats).ok_or_else(|| {
        NativeError::Surface("surface exposes no compatible texture formats".to_owned())
    })?;
    let present_mode = choose_present_mode(&capabilities.present_modes).ok_or_else(|| {
        NativeError::Surface("surface exposes no compatible presentation modes".to_owned())
    })?;
    let alpha_mode = choose_alpha_mode(&capabilities.alpha_modes).ok_or_else(|| {
        NativeError::Surface("surface exposes no compatible alpha modes".to_owned())
    })?;
    let mut configuration = surface
        .get_default_config(adapter, width, height)
        .ok_or_else(|| NativeError::Surface("surface has no default configuration".to_owned()))?;
    configuration.format = format;
    configuration.present_mode = present_mode;
    configuration.alpha_mode = alpha_mode;
    validate_surface_configuration(&capabilities, &configuration)?;
    Ok(configuration)
}

struct GraphicsState {
    window: Arc<Window>,
    context: GpuContext,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    scene_texture: Option<wgpu::Texture>,
    scene_bind_group: Option<wgpu::BindGroup>,
    scene_renderer: SceneRenderer,
    composite_pipeline: wgpu::RenderPipeline,
    composite_bind_group_layout: wgpu::BindGroupLayout,
    composite_sampler: wgpu::Sampler,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,

    egui_renderer: EguiRenderer,
}

/// Owns a frame's texture delta until it is submitted or follows the
/// documented non-render recovery path. Clearing on drop prevents epaint's
/// debug assertion if an unexpected early return occurs.
struct PendingTextureDelta {
    delta: egui::TexturesDelta,
}

impl Drop for PendingTextureDelta {
    fn drop(&mut self) {
        self.delta.clear();
    }
}

impl GraphicsState {
    fn new(window: &Arc<Window>, viewport: crate::Viewport) -> Result<Self, NativeError> {
        if viewport.width == 0 || viewport.height == 0 {
            return Err(NativeError::Surface(
                "zero-sized viewport cannot configure surface".to_owned(),
            ));
        }
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(Arc::clone(window))
            .map_err(|error| NativeError::Surface(error.to_string()))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .map_err(|error| NativeError::Gpu(format!("no compatible adapter: {error}")))?;
        let descriptor = wgpu::DeviceDescriptor {
            label: Some("gource-app-device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            trace: wgpu::Trace::Off,
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&descriptor))
            .map_err(|error| NativeError::Gpu(error.to_string()))?;
        let context = GpuContext::from_parts(instance, adapter, device, queue);
        // The context owns the instance/adapter; the existing surface remains
        // valid.  Build the configuration from the adapter that selected the
        // device, without creating another device or queue.
        let surface_config =
            surface_configuration(&surface, context.adapter(), viewport.width, viewport.height)?;
        surface.configure(context.device(), &surface_config);

        let egui_ctx = egui::Context::default();
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        let egui_renderer = EguiRenderer::new(
            context.device(),
            surface_config.format,
            EguiRendererOptions::default(),
        );
        let composite_bind_group_layout =
            context
                .device()
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("gource-app-composite-bind-group"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                                view_dimension: wgpu::TextureViewDimension::D2,
                                multisampled: false,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                            count: None,
                        },
                    ],
                });
        let composite_layout =
            context
                .device()
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("gource-app-composite-layout"),
                    bind_group_layouts: &[Some(&composite_bind_group_layout)],
                    immediate_size: 0,
                });
        let composite_shader =
            context
                .device()
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("gource-app-composite-shader"),
                    source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(COMPOSITE_WGSL)),
                });
        let composite_pipeline =
            context
                .device()
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("gource-app-composite-pipeline"),
                    layout: Some(&composite_layout),
                    vertex: wgpu::VertexState {
                        module: &composite_shader,
                        entry_point: Some("vs_main"),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        buffers: &[],
                    },
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    fragment: Some(wgpu::FragmentState {
                        module: &composite_shader,
                        entry_point: Some("fs_main"),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: surface_config.format,
                            blend: None,
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    multiview_mask: None,
                    cache: None,
                });
        let composite_sampler = context.device().create_sampler(&wgpu::SamplerDescriptor {
            label: Some("gource-app-composite-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let scene_config = RendererConfig {
            view: RenderView {
                width: viewport.width,
                height: viewport.height,
                ..RenderView::default()
            },
            ..RendererConfig::default()
        };
        let scene_renderer = SceneRenderer::new(&context, scene_config)?;
        let mut state = Self {
            window: Arc::clone(window),
            context,
            surface,
            surface_config,
            scene_texture: None,
            scene_bind_group: None,
            scene_renderer,
            composite_pipeline,
            composite_bind_group_layout,
            composite_sampler,
            egui_ctx,
            egui_state,
            egui_renderer,
        };
        state.recreate_scene_target(viewport.width, viewport.height);
        Ok(state)
    }

    fn resize(&mut self, size: PhysicalSize<u32>) -> Result<(), NativeError> {
        let policy = resize_policy(size);
        if policy.release_scene_texture {
            self.scene_texture = None;
        }
        if policy.release_scene_bind_group {
            self.scene_bind_group = None;
        }
        if !policy.configure_surface {
            // wgpu::Surface::configure rejects zero extents.  Keep the last
            // valid configuration so the next non-zero resize can restore it,
            // but release all size-dependent scene resources while minimized.
            return Ok(());
        }
        validate_surface_extent(self.context.adapter(), size.width, size.height)?;
        let mut configuration = self.surface_config.clone();
        configuration.width = size.width;
        configuration.height = size.height;
        let capabilities = self.surface.get_capabilities(self.context.adapter());
        validate_surface_configuration(&capabilities, &configuration)?;
        self.surface
            .configure(self.context.device(), &configuration);
        self.surface_config = configuration;
        self.recreate_scene_target(size.width, size.height);
        Ok(())
    }

    fn is_renderable(&self) -> bool {
        self.surface_config.width > 0
            && self.surface_config.height > 0
            && self.scene_texture.is_some()
            && self.scene_bind_group.is_some()
    }

    fn recreate_scene_target(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            self.scene_texture = None;
            self.scene_bind_group = None;
            return;
        }
        let texture = self
            .context
            .device()
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("gource-app-canonical-scene-target"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: RENDER_TARGET_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self
            .context
            .device()
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("gource-app-composite-bind-group"),
                layout: &self.composite_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.composite_sampler),
                    },
                ],
            });
        self.scene_texture = Some(texture);
        self.scene_bind_group = Some(bind_group);
    }

    fn render(
        &mut self,
        window: &Window,
        snapshot: &SceneSnapshot,
        catalog: &Catalog,
        camera: RenderView,
        ui_data: UiData,
    ) -> Result<(Vec<AppCommand>, RenderOutcome), NativeError> {
        if camera.width == 0 || camera.height == 0 {
            return Ok((Vec::new(), RenderOutcome::Occluded));
        }
        if camera.width != self.surface_config.width || camera.height != self.surface_config.height
        {
            self.resize(PhysicalSize::new(camera.width, camera.height))?;
        }
        if !self.is_renderable() {
            return Ok((Vec::new(), RenderOutcome::Occluded));
        }
        self.scene_renderer.set_view(camera)?;
        let raw_input = self.egui_state.take_egui_input(window);
        let mut commands = Vec::new();
        let full_output = self.egui_ctx.run_ui(raw_input, |ui| {
            draw_ui(ui.ctx(), &ui_data, &mut commands);
        });
        let repaint_delay = full_output
            .viewport_output
            .values()
            .map(|output| output.repaint_delay)
            .min()
            .unwrap_or(Duration::MAX);
        self.egui_state
            .handle_platform_output(window, full_output.platform_output);
        let paint_jobs = self
            .egui_ctx
            .tessellate(full_output.shapes, full_output.pixels_per_point);
        let screen_descriptor = ScreenDescriptor {
            size_in_pixels: [self.surface_config.width, self.surface_config.height],
            pixels_per_point: full_output.pixels_per_point,
        };
        let mut textures_delta = PendingTextureDelta {
            delta: full_output.textures_delta,
        };
        let mut texture_uploads_queued = false;
        for (id, deltas) in &textures_delta.delta.set {
            for delta in deltas {
                self.egui_renderer.update_texture(
                    self.context.device(),
                    self.context.queue(),
                    *id,
                    delta,
                );
                texture_uploads_queued = true;
            }
        }
        // The set deltas have been applied before surface acquisition. Keep
        // only the frees until it is known whether this frame was submitted.
        textures_delta.delta.set.clear();
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Timeout => {
                self.free_texture_delta(&mut textures_delta.delta, texture_uploads_queued);
                return Ok((
                    commands,
                    RenderOutcome::Retry {
                        delay: SURFACE_RETRY_DELAY,
                    },
                ));
            }
            wgpu::CurrentSurfaceTexture::Occluded => {
                self.free_texture_delta(&mut textures_delta.delta, texture_uploads_queued);
                return Ok((commands, RenderOutcome::Occluded));
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.free_texture_delta(&mut textures_delta.delta, texture_uploads_queued);
                self.reconfigure_surface(false)?;
                return Ok((
                    commands,
                    RenderOutcome::Retry {
                        delay: SURFACE_RETRY_DELAY,
                    },
                ));
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.free_texture_delta(&mut textures_delta.delta, texture_uploads_queued);
                self.reconfigure_surface(true)?;
                return Ok((
                    commands,
                    RenderOutcome::Retry {
                        delay: SURFACE_RETRY_DELAY,
                    },
                ));
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                self.free_texture_delta(&mut textures_delta.delta, texture_uploads_queued);
                return Err(NativeError::Surface(
                    "surface acquisition failed validation".to_owned(),
                ));
            }
        };
        let surface_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let Some(scene_texture) = self.scene_texture.as_ref() else {
            self.free_texture_delta(&mut textures_delta.delta, texture_uploads_queued);
            return Err(NativeError::Surface(
                "canonical scene target is unavailable".to_owned(),
            ));
        };
        let scene_view = scene_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let Some(scene_bind_group) = self.scene_bind_group.as_ref() else {
            self.free_texture_delta(&mut textures_delta.delta, texture_uploads_queued);
            return Err(NativeError::Surface(
                "canonical scene bind group is unavailable".to_owned(),
            ));
        };
        let mut encoder =
            self.context
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("gource-app-frame-encoder"),
                });
        if let Err(error) = self.scene_renderer.render_to_target(
            snapshot,
            catalog,
            &mut encoder,
            RenderTarget::new(
                &scene_view,
                self.surface_config.width,
                self.surface_config.height,
                RENDER_TARGET_FORMAT,
                1,
            ),
        ) {
            self.free_texture_delta(&mut textures_delta.delta, texture_uploads_queued);
            return Err(error.into());
        }
        {
            let color_attachments = [Some(wgpu::RenderPassColorAttachment {
                view: &surface_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })];
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("gource-app-composite-pass"),
                color_attachments: &color_attachments,
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.composite_pipeline);
            pass.set_bind_group(0, scene_bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        let callback_commands = self.egui_renderer.update_buffers(
            self.context.device(),
            self.context.queue(),
            &mut encoder,
            &paint_jobs,
            &screen_descriptor,
        );
        {
            let color_attachments = [Some(wgpu::RenderPassColorAttachment {
                view: &surface_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })];
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("gource-app-egui-pass"),
                color_attachments: &color_attachments,
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let mut pass = pass.forget_lifetime();
            self.egui_renderer
                .render(&mut pass, &paint_jobs, &screen_descriptor);
        }
        let frame_command = encoder.finish();
        let mut commands_to_submit = callback_commands;
        commands_to_submit.push(frame_command);
        self.context.queue().submit(commands_to_submit);
        texture_uploads_queued = false;
        self.context.queue().present(frame);
        self.free_texture_delta(&mut textures_delta.delta, texture_uploads_queued);
        Ok((commands, RenderOutcome::Presented { repaint_delay }))
    }

    fn free_texture_delta(
        &mut self,
        textures_delta: &mut egui::TexturesDelta,
        texture_uploads_queued: bool,
    ) {
        if texture_uploads_queued {
            self.context.queue().submit(std::iter::empty());
        }
        for id in &textures_delta.free {
            self.egui_renderer.free_texture(id);
        }
        textures_delta.clear();
    }

    fn reconfigure_surface(&mut self, recreate: bool) -> Result<(), NativeError> {
        if self.surface_config.width == 0 || self.surface_config.height == 0 {
            self.scene_texture = None;
            self.scene_bind_group = None;
            return Ok(());
        }
        validate_surface_extent(
            self.context.adapter(),
            self.surface_config.width,
            self.surface_config.height,
        )?;
        if recreate {
            // A lost surface can retain stale platform handles.  Construct
            // and validate the replacement before publishing it so the old
            // surface remains usable if creation fails.
            let surface = self
                .context
                .instance()
                .create_surface(Arc::clone(&self.window))
                .map_err(|error| NativeError::Surface(error.to_string()))?;
            let capabilities = surface.get_capabilities(self.context.adapter());
            validate_surface_configuration(&capabilities, &self.surface_config)?;
            surface.configure(self.context.device(), &self.surface_config);
            self.surface = surface;
        } else {
            let capabilities = self.surface.get_capabilities(self.context.adapter());
            validate_surface_configuration(&capabilities, &self.surface_config)?;
            self.surface
                .configure(self.context.device(), &self.surface_config);
        }
        if !self.is_renderable() {
            self.recreate_scene_target(self.surface_config.width, self.surface_config.height);
        }
        Ok(())
    }
}

fn draw_ui(ctx: &egui::Context, data: &UiData, commands: &mut Vec<AppCommand>) {
    egui::Window::new("Transport")
        .resizable(false)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .button(if data.paused { "Play" } else { "Pause" })
                    .clicked()
                {
                    commands.push(AppCommand::TogglePause);
                }
                if ui.button("Next event").clicked() {
                    commands.push(AppCommand::NextEvent);
                }
                if ui.button("Overview").clicked() {
                    commands.push(AppCommand::SetCameraMode(CameraControlMode::Overview));
                }
                if ui.button("Track").clicked() {
                    commands.push(AppCommand::SetCameraMode(CameraControlMode::Track));
                }
                if ui.button("Reset camera").clicked() {
                    commands.push(AppCommand::ResetCamera);
                }
                ui.label(format!("tick {}", data.tick));
                ui.label(format!("{}× playback", data.playback_rate));
            });
        });
    egui::Window::new("Timeline")
        .resizable(false)
        .show(ctx, |ui| {
            let mut tick = data.tick.min(data.total_ticks.max(1));
            if ui
                .add(egui::Slider::new(&mut tick, 0..=data.total_ticks.max(1)).text("timeline"))
                .changed()
            {
                commands.push(AppCommand::SeekTick(tick));
            }
        });
    egui::Window::new("Settings")
        .resizable(true)
        .show(ctx, |ui| {
            let mut rate = data.playback_rate;
            if ui
                .add(
                    egui::Slider::new(&mut rate, 0.05..=64.0)
                        .logarithmic(true)
                        .text("rate"),
                )
                .changed()
            {
                commands.push(AppCommand::SetPlaybackRate(rate));
            }
            ui.label(format!("path filters: {}", data.filter_count));
            if let Some(label) = &data.selected_label {
                ui.label(format!("selected: {label}"));
            } else {
                ui.label("selected: none");
            }
            ui.label("Space/P pause · N/right next · right drag pan · wheel zoom");
        });
}

fn should_request_redraw(command_batch_nonempty: bool, egui_repaint: bool) -> bool {
    command_batch_nonempty || egui_repaint
}

fn repaint_deadline(now: Instant, delay: Duration) -> Option<Instant> {
    if delay.is_zero() || delay == Duration::MAX {
        None
    } else {
        now.checked_add(delay)
    }
}

fn scale_duration(duration: Duration, rate: f64) -> Duration {
    if !rate.is_finite() || rate <= 0.0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64((duration.as_secs_f64() * rate).min(Duration::MAX.as_secs_f64()))
}

fn glob_match(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;
    for token in pattern {
        let mut current = vec![false; value.len() + 1];
        if token == '*' {
            current[0] = previous[0];
            for index in 1..=value.len() {
                current[index] = previous[index] || current[index - 1];
            }
        } else {
            for index in 1..=value.len() {
                current[index] = previous[index - 1] && (token == '?' || token == value[index - 1]);
            }
        }
        previous = current;
    }
    previous[value.len()]
}

const COMPOSITE_WGSL: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    var uvs = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(2.0, 1.0),
        vec2<f32>(0.0, -1.0),
    );
    var output: VertexOutput;
    output.position = vec4<f32>(positions[index], 0.0, 1.0);
    output.uv = uvs[index];
    return output;
}

@group(0) @binding(0) var source_texture: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, input.uv);
}
"#;

/// Errors raised while loading or presenting a native scene.
#[derive(Debug, Error)]
pub enum NativeError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Ingest(#[from] gource_ingest::IngestError),
    #[error(transparent)]
    Replay(#[from] ReplayError),
    #[error(transparent)]
    Cache(#[from] gource_ingest::CacheError),
    #[error(transparent)]
    Renderer(#[from] gource_render::RendererError),
    #[error("event loop failed: {0}")]
    EventLoop(String),
    #[error("window creation failed: {0}")]
    Window(String),
    #[error("surface operation failed: {0}")]
    Surface(String),
    #[error("GPU initialization failed: {0}")]
    Gpu(String),
    #[error("export failed: {0}")]
    Export(String),
    #[error("{0} command cannot run in this mode")]
    WrongCommand(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_directory(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "gource-app-native-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("test directory");
        path
    }

    fn cache_entry_count(path: &std::path::Path) -> usize {
        fs::read_dir(path)
            .expect("cache directory")
            .flatten()
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "bin")
            })
            .count()
    }

    #[test]
    fn glob_filter_is_deterministic_and_bounded() {
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(glob_match("**/main.?s", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "tests/main.log"));
    }

    #[test]
    fn zero_viewport_skips_surface_configuration_and_releases_scene_resources() {
        let zero_width = resize_policy(PhysicalSize::new(0, 720));
        assert_eq!(
            zero_width,
            ResizePolicy {
                configure_surface: false,
                release_scene_texture: true,
                release_scene_bind_group: true,
            }
        );

        let zero_height = resize_policy(PhysicalSize::new(1280, 0));
        assert_eq!(zero_height, zero_width);

        let visible = resize_policy(PhysicalSize::new(1280, 720));
        assert_eq!(
            visible,
            ResizePolicy {
                configure_surface: true,
                release_scene_texture: false,
                release_scene_bind_group: false,
            }
        );
    }

    #[test]
    fn repository_second_seek_preserves_signed_fraction_exactly() {
        let current = RationalTime::new(-3, 2);
        assert_eq!(
            repository_time_after_seconds(current, -0.1).unwrap(),
            RationalTime::new(-8, 5)
        );
        assert_eq!(
            repository_time_after_seconds(current, 0.1).unwrap(),
            RationalTime::new(-7, 5)
        );
    }

    #[test]
    fn next_event_comparison_keeps_negative_fractional_positions_exact() {
        let current = RationalTime::new(-3, 2);
        assert!(!event_timestamp_is_after(-2, current));
        assert!(event_timestamp_is_after(-1, current));
        assert!(!event_timestamp_is_after(-1, RationalTime::new(-1, 1)));
        assert!(event_timestamp_is_after(0, RationalTime::new(-1, 1)));
        assert!(!event_timestamp_is_after(1, RationalTime::new(3, 2)));
        assert!(event_timestamp_is_after(2, RationalTime::new(3, 2)));
    }

    #[test]
    fn unix_repository_times_map_to_playback_relative_wall_time() {
        let config = AppConfig::default();
        let origin = 1_700_000_000;
        let rate = validated_repository_rate(&config, origin).expect("default replay rate");
        assert_eq!(
            playback_relative_time(Rational::new(origin as i128, 1).unwrap(), origin, rate,)
                .unwrap(),
            Rational::ZERO
        );
        assert_eq!(
            playback_relative_time(
                Rational::new((origin + 86_400) as i128, 1).unwrap(),
                origin,
                rate,
            )
            .unwrap(),
            Rational::new(10, 1).unwrap()
        );
    }

    #[test]
    fn default_one_repository_day_is_1200_ticks() {
        let origin = 1_700_000_000;
        let rate =
            validated_repository_rate(&AppConfig::default(), origin).expect("default replay rate");
        assert_eq!(
            playback_timestamp_tick(origin + 86_400, origin, rate).unwrap(),
            1_200
        );
    }

    #[test]
    fn default_one_repository_second_ceil_starts_at_first_applying_tick() {
        let origin = 1_700_000_000;
        let rate =
            validated_repository_rate(&AppConfig::default(), origin).expect("default replay rate");
        assert_eq!(
            playback_timestamp_tick(origin + 1, origin, rate).unwrap(),
            1
        );
    }

    #[test]
    fn default_exact_tick_boundary_stays_exact() {
        let origin = 1_700_000_000;
        let rate =
            validated_repository_rate(&AppConfig::default(), origin).expect("default replay rate");
        assert_eq!(
            playback_timestamp_tick(origin + 72, origin, rate).unwrap(),
            1
        );
    }

    #[test]
    fn pre_origin_timestamp_clamps_to_origin_tick() {
        let origin = 1_700_000_000;
        let rate =
            validated_repository_rate(&AppConfig::default(), origin).expect("default replay rate");
        assert_eq!(
            playback_timestamp_tick(origin - 1, origin, rate).unwrap(),
            0
        );
        assert_eq!(playback_timestamp_tick(origin, origin, rate).unwrap(), 0);
    }

    #[test]
    fn realtime_rate_is_one_repository_second_per_wall_second() {
        let mut config = AppConfig::default();
        config.replay.realtime = true;
        config.replay.seconds_per_day = 86_400.0;
        let origin = 1_700_000_000;
        let rate = validated_repository_rate(&config, origin).expect("realtime replay rate");
        assert_eq!(rate, Rational::ONE);
        assert_eq!(
            playback_timestamp_tick(origin + 5, origin, rate).unwrap(),
            600
        );
    }

    #[test]
    fn reset_camera_restores_overview_without_changing_replay_or_pause() {
        let history =
            gource_ingest::parse_bytes(b"0|alice|A|src/main.rs\n", ()).expect("valid history");
        let mut app =
            NativeApplication::new(AppConfig::default(), FilteredHistory::new(history, &[]))
                .expect("native application");
        app.input.paused = true;
        app.input.playback_rate = 3.0;
        app.input.set_viewport(1920, 1080);
        app.input.camera.mode = CameraControlMode::Manual;
        app.input.camera.center = [4.0, -2.0];
        app.input.camera.zoom = 5.0;
        app.input.camera.rotation_radians = 0.25;
        app.input.right_dragging = true;
        app.input.left_selecting = true;
        app.redraw_requested = false;
        let snapshot = app.replay.snapshot();

        app.reset_camera();

        assert_eq!(app.replay.snapshot(), snapshot);
        assert!(app.input.paused);
        assert_eq!(app.input.playback_rate, 3.0);
        assert_eq!(app.input.camera.mode, CameraControlMode::Overview);
        assert_eq!(app.input.camera.center, [0.0, 0.0]);
        assert_eq!(app.input.camera.zoom, 1.0);
        assert_eq!(app.input.camera.rotation_radians, 0.0);
        assert_eq!(app.input.camera.viewport, [1920, 1080]);
        assert!(!app.input.right_dragging);
        assert!(!app.input.left_selecting);
        assert!(app.redraw_requested);
    }

    #[test]
    fn paused_idle_frames_do_not_request_redraw_without_effective_work() {
        assert!(!should_request_redraw(false, false));
        assert!(should_request_redraw(true, false));
        assert!(should_request_redraw(false, true));
    }

    #[test]
    fn surface_selection_prefers_srgb_fifo_and_opaque_when_available() {
        assert_eq!(
            choose_surface_format(&[
                wgpu::TextureFormat::Rgba8Unorm,
                wgpu::TextureFormat::Bgra8UnormSrgb,
            ]),
            Some(wgpu::TextureFormat::Bgra8UnormSrgb)
        );
        assert_eq!(
            choose_present_mode(&[wgpu::PresentMode::Immediate, wgpu::PresentMode::Fifo]),
            Some(wgpu::PresentMode::Fifo)
        );
        assert_eq!(
            choose_alpha_mode(&[
                wgpu::CompositeAlphaMode::Auto,
                wgpu::CompositeAlphaMode::Opaque,
            ]),
            Some(wgpu::CompositeAlphaMode::Opaque)
        );
    }

    #[test]
    fn surface_selection_falls_back_to_the_first_supported_value() {
        assert_eq!(
            choose_surface_format(&[wgpu::TextureFormat::Rgba16Float]),
            Some(wgpu::TextureFormat::Rgba16Float)
        );
        assert_eq!(
            choose_present_mode(&[wgpu::PresentMode::Immediate]),
            Some(wgpu::PresentMode::Immediate)
        );
        assert_eq!(
            choose_alpha_mode(&[wgpu::CompositeAlphaMode::Inherit]),
            Some(wgpu::CompositeAlphaMode::Inherit)
        );
        assert_eq!(choose_surface_format(&[]), None);
        assert_eq!(choose_present_mode(&[]), None);
        assert_eq!(choose_alpha_mode(&[]), None);
    }

    #[test]
    fn transient_repaint_deadlines_are_bounded_and_special_delays_do_not_wake() {
        let now = Instant::now();
        assert!(repaint_deadline(now, SURFACE_RETRY_DELAY).is_some_and(|deadline| deadline > now));
        assert_eq!(repaint_deadline(now, Duration::ZERO), None);
        assert_eq!(repaint_deadline(now, Duration::MAX), None);
        assert_eq!(
            RenderOutcome::Retry {
                delay: SURFACE_RETRY_DELAY
            },
            RenderOutcome::Retry {
                delay: Duration::from_millis(16)
            }
        );
    }

    #[test]
    fn regular_file_cache_is_opt_in_and_keys_bytes_and_options() {
        let root = test_directory("cache-keys");
        let input = root.join("history.log");
        let cache = root.join("cache");
        fs::write(&input, b"1|alice|A|src/main.rs\n").expect("history input");

        let config = AppConfig {
            input: InputSpec::file(input.clone()),
            cache_dir: Some(cache.clone()),
            cache_bytes: 8 * 1024 * 1024,
            ..Default::default()
        };

        let first = load_history(&config).expect("first load");
        let second = load_history(&config).expect("cached load");
        assert_eq!(
            first.base().dataset_identity(),
            second.base().dataset_identity()
        );
        assert_eq!(cache_entry_count(&cache), 1);

        let mut changed_options = config.clone();
        changed_options.replay.limits.path_bytes = 1024;
        load_history(&changed_options).expect("changed options load");
        assert_eq!(cache_entry_count(&cache), 2);

        fs::write(&input, b"2|alice|A|src/main.rs\n").expect("changed history input");
        let changed_bytes = load_history(&config).expect("changed bytes load");
        assert_ne!(
            first.base().input_identity(),
            changed_bytes.base().input_identity()
        );
        assert_eq!(cache_entry_count(&cache), 3);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn corrupt_cache_entry_is_reparsed_and_republished() {
        let root = test_directory("cache-corrupt");
        let input = root.join("history.log");
        let cache = root.join("cache");
        fs::write(&input, b"1|alice|A|src/main.rs\n").expect("history input");

        let config = AppConfig {
            input: InputSpec::file(input),
            cache_dir: Some(cache.clone()),
            cache_bytes: 8 * 1024 * 1024,
            ..Default::default()
        };
        let expected = load_history(&config).expect("first load");
        let entry = fs::read_dir(&cache)
            .expect("cache directory")
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|extension| extension == "bin"))
            .expect("cache entry");
        fs::write(entry, b"corrupt").expect("corrupt cache entry");

        let recovered = load_history(&config).expect("reparse corrupt entry");
        assert_eq!(
            expected.base().dataset_identity(),
            recovered.base().dataset_identity()
        );
        assert_eq!(cache_entry_count(&cache), 1);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn missing_cache_directory_has_no_side_effects() {
        let root = test_directory("cache-disabled");
        let input = root.join("history.log");
        let cache = root.join("cache");
        fs::write(&input, b"1|alice|A|src/main.rs\n").expect("history input");

        let config = AppConfig {
            input: InputSpec::file(input),
            ..Default::default()
        };
        load_history(&config).expect("uncached load");
        assert!(!cache.exists());
        fs::remove_dir_all(root).expect("remove test directory");
    }
}
