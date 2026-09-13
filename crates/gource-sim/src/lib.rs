// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Deterministic, fixed-step repository-history simulation.
//!
//! `ReplaySession` is deliberately the only mutable owner of replay state.  It
//! consumes an immutable [`gource_core::HistorySource`], applies events in
//! canonical order, and publishes compact, renderer-independent snapshots.
//! No window, UI, or graphics type is used in this crate.

use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use gource_core::{
    Action, Catalog, ContributorId, DirId, Event, EventTarget, FileId, Generation, HistorySource,
    PathId, PlaybackClock, Rational, ReplayConfig, Rgb8, Tick, World, WorldError,
};
use rayon::ThreadPool;
use rayon::ThreadPoolBuilder;

/// The canonical simulation frequency selected by the core contract.
pub const SIMULATION_HZ: u64 = 120;
const ACTION_TRAVEL_TICKS: u64 = 30;
const ACTION_LIFETIME_TICKS: u64 = 180;
const FILE_FADE_TICKS: u64 = 90;
const COLLISION_DISTANCE: f32 = 0.38;
const MAX_COLLISION_PASSES: usize = 16;
const MAX_CHECKPOINTS: usize = 64;
const MAX_SNAPSHOT_BUFFERS: usize = 3;
const MIN_PARALLEL_PAIR_WORK: u128 = 512;
const FILE_PALETTE: [Rgb8; 16] = [
    Rgb8::new(61, 181, 232),
    Rgb8::new(236, 170, 54),
    Rgb8::new(203, 101, 231),
    Rgb8::new(71, 205, 135),
    Rgb8::new(235, 92, 118),
    Rgb8::new(103, 143, 230),
    Rgb8::new(239, 130, 62),
    Rgb8::new(59, 194, 202),
    Rgb8::new(194, 209, 77),
    Rgb8::new(151, 116, 225),
    Rgb8::new(73, 184, 155),
    Rgb8::new(232, 111, 188),
    Rgb8::new(112, 207, 228),
    Rgb8::new(222, 149, 89),
    Rgb8::new(158, 203, 93),
    Rgb8::new(188, 135, 224),
];
const DIRECTORY_COLOR: Rgb8 = Rgb8::new(119, 139, 164);
const ACTION_ADD_COLOR: Rgb8 = Rgb8::new(73, 211, 119);
const ACTION_MODIFY_COLOR: Rgb8 = Rgb8::new(244, 164, 52);
const ACTION_DELETE_COLOR: Rgb8 = Rgb8::new(235, 78, 88);

/// Selects the execution strategy used by a replay session.
///
/// `Serial` is the canonical reference implementation.  `Parallel` retains
/// one explicitly bounded Rayon pool for the lifetime of the session and only
/// schedules complete same-parent groups when a pair-work grain can be split
/// across disjoint slices.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionMode {
    /// Run every simulation stage on the calling thread.
    Serial,
    /// Use a bounded, session-local Rayon pool.
    Parallel { threads: NonZeroUsize },
}

impl ExecutionMode {
    #[must_use]
    pub const fn configured_threads(self) -> usize {
        match self {
            Self::Serial => 1,
            Self::Parallel { threads } => threads.get(),
        }
    }
}

/// A small rational value used for repository time and snapshot timestamps.
///
/// Values are normalized to a positive denominator when [`Self::reduced`] is
/// called.  Raw construction remains available for `const` callers, while
/// normalization is fallible for signed `i128` edge cases that have no
/// representable reduced form.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd)]
pub struct RationalTime {
    /// Signed numerator.  Raw values may be unnormalized.
    pub numerator: i128,
    /// Denominator, normalized to positive by [`Self::reduced`].
    pub denominator: i128,
}

impl RationalTime {
    /// Creates a raw rational value.
    ///
    /// Construction intentionally does not inspect the denominator so this
    /// function remains usable in `const` contexts.  Call [`Self::reduced`]
    /// before using a value in simulation arithmetic; normalization is
    /// fallible because some signed `i128` pairs have no equivalent form with
    /// an `i128` numerator and positive `i128` denominator.
    #[must_use]
    pub const fn new(numerator: i128, denominator: i128) -> Self {
        Self {
            numerator,
            denominator,
        }
    }

    /// Returns a normalized value with a positive denominator.
    ///
    /// The absolute value of `i128::MIN` is representable only as `u128`.
    /// Reduction therefore works in unsigned magnitudes and reports
    /// [`ReplayError::RationalTimeOutOfRange`] when the reduced result cannot
    /// be represented by this type.
    pub fn reduced(self) -> Result<Self, ReplayError> {
        if self.denominator == 0 {
            return Err(ReplayError::RationalTimeOutOfRange);
        }
        if self.numerator == 0 {
            return Ok(Self::new(0, 1));
        }

        let numerator_magnitude = self.numerator.unsigned_abs();
        let denominator_magnitude = self.denominator.unsigned_abs();
        let divisor = gcd_i128(numerator_magnitude, denominator_magnitude);
        let numerator_magnitude = numerator_magnitude / divisor;
        let denominator_magnitude = denominator_magnitude / divisor;
        let denominator = i128::try_from(denominator_magnitude)
            .map_err(|_| ReplayError::RationalTimeOutOfRange)?;
        let negative = (self.numerator < 0) ^ (self.denominator < 0);
        let numerator = if negative {
            if numerator_magnitude == 1u128 << 127 {
                i128::MIN
            } else {
                let magnitude = i128::try_from(numerator_magnitude)
                    .map_err(|_| ReplayError::RationalTimeOutOfRange)?;
                -magnitude
            }
        } else {
            i128::try_from(numerator_magnitude).map_err(|_| ReplayError::RationalTimeOutOfRange)?
        };
        Ok(Self::new(numerator, denominator))
    }

    /// Converts to seconds for presentation-only calculations.
    #[must_use]
    pub fn as_f64(self) -> f64 {
        self.numerator as f64 / self.denominator as f64
    }

    /// Adds two rational values without going through floating point.
    pub fn checked_add(self, other: Self) -> Result<Self, ReplayError> {
        let left = self.reduced()?;
        let right = other.reduced()?;
        let numerator = left
            .numerator
            .checked_mul(right.denominator)
            .and_then(|value| {
                right
                    .numerator
                    .checked_mul(left.denominator)
                    .and_then(|other| value.checked_add(other))
            })
            .ok_or(ReplayError::RationalTimeOutOfRange)?;
        let denominator = left
            .denominator
            .checked_mul(right.denominator)
            .ok_or(ReplayError::RationalTimeOutOfRange)?;
        Self::new(numerator, denominator).reduced()
    }
}

impl Default for RationalTime {
    fn default() -> Self {
        Self {
            numerator: 0,
            denominator: 1,
        }
    }
}

impl From<Rational> for RationalTime {
    fn from(value: Rational) -> Self {
        Self::new(value.numerator, value.denominator)
    }
}

fn gcd_i128(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let remainder = a % b;
        a = b;
        b = remainder;
    }
    if a == 0 { 1 } else { a }
}

/// A two-dimensional position used by every prepared visual.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    #[must_use]
    fn length(self) -> f32 {
        self.x.hypot(self.y)
    }

    #[must_use]
    fn add(self, other: Self) -> Self {
        Self::new(self.x + other.x, self.y + other.y)
    }

    #[must_use]
    fn sub(self, other: Self) -> Self {
        Self::new(self.x - other.x, self.y - other.y)
    }

    #[must_use]
    fn scale(self, amount: f32) -> Self {
        Self::new(self.x * amount, self.y * amount)
    }

    #[must_use]
    fn clamped_radius(self, maximum: f32) -> Self {
        let radius = self.length();
        if !radius.is_finite() {
            return Self::default();
        }
        if radius <= maximum || radius <= f32::EPSILON {
            self
        } else {
            self.scale(maximum / radius)
        }
    }
}

const MAX_LAYOUT_RADIUS: f32 = 24.0;
const MAX_LAYOUT_SPEED: f32 = 0.18;

/// Prepared directory geometry.
#[derive(Clone, Debug, PartialEq)]
pub struct DirectoryVisual {
    /// Stable compressed-hierarchy identity allocated by [`World`].
    pub id: DirId,
    pub position: Vec2,
    pub radius: f32,
    pub opacity: f32,
    pub color: Rgb8,
}

/// Prepared file geometry and lifecycle state.
#[derive(Clone, Debug, PartialEq)]
pub struct FileVisual {
    /// Stable lexical path identity.
    pub path_id: PathId,
    /// Stable file-incarnation identity allocated by [`World`].
    pub file_id: FileId,
    /// A new incarnation is assigned after a replacement or recreation.
    pub incarnation: u64,
    pub position: Vec2,
    pub radius: f32,
    pub opacity: f32,
    pub active: bool,
    pub color: Rgb8,
}

/// Prepared contributor geometry.
#[derive(Clone, Debug, PartialEq)]
pub struct ContributorVisual {
    pub id: ContributorId,
    pub position: Vec2,
    pub energy: f32,
    pub active: bool,
    pub color: Rgb8,
}

/// A contributor-to-file motion trail prepared for rendering.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionVisual {
    pub contributor: ContributorId,
    pub path_id: PathId,
    pub action: Action,
    pub position: Vec2,
    pub target: Vec2,
    pub progress: f32,
    pub opacity: f32,
    pub color: Rgb8,
}

/// A typed node identity used by hierarchy branches.  Directory and file
/// incarnation namespaces are intentionally not interchangeable.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HierarchyNodeId {
    Directory(DirId),
    File(FileId),
}

/// A branch between two prepared hierarchy points.
#[derive(Clone, Debug, PartialEq)]
pub struct BranchVisual {
    pub parent: Option<DirId>,
    pub child: HierarchyNodeId,
    pub start: Vec2,
    pub end: Vec2,
    pub opacity: f32,
    pub color: Rgb8,
}

/// A bounded text label prepared by simulation rather than by a renderer.
#[derive(Clone, Debug, PartialEq)]
pub struct LabelVisual {
    pub target: PathId,
    pub text: String,
    pub position: Vec2,
    pub opacity_bits: u32,
    pub priority: u8,
    pub visible: bool,
}

impl LabelVisual {
    #[must_use]
    pub fn opacity(&self) -> f32 {
        f32::from_bits(self.opacity_bits)
    }
}

/// Finite axis-aligned scene rectangle used by overview cameras and exports.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneBounds {
    pub min: Vec2,
    pub max: Vec2,
}

impl SceneBounds {
    #[must_use]
    pub const fn new(min: Vec2, max: Vec2) -> Self {
        Self { min, max }
    }

    #[must_use]
    pub fn width(self) -> f32 {
        (self.max.x - self.min.x).abs()
    }

    #[must_use]
    pub fn height(self) -> f32 {
        (self.max.y - self.min.y).abs()
    }

    #[must_use]
    pub fn center(self) -> Vec2 {
        Vec2::new(
            (self.min.x + self.max.x) * 0.5,
            (self.min.y + self.max.y) * 0.5,
        )
    }

    #[must_use]
    pub fn as_array(self) -> [f32; 4] {
        [self.min.x, self.min.y, self.max.x, self.max.y]
    }

    #[must_use]
    pub fn as_f64(self) -> [f64; 4] {
        [
            f64::from(self.min.x),
            f64::from(self.min.y),
            f64::from(self.max.x),
            f64::from(self.max.y),
        ]
    }
}

/// All data required to render one complete scene.
///
/// The vectors are ordered by stable identity.  A snapshot is immutable once
/// published and can therefore be retained by a renderer or exporter without
/// borrowing a mutable world.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneSnapshot {
    pub tick: u64,
    pub repository_time: RationalTime,
    pub generation: u64,
    pub directories: Vec<DirectoryVisual>,
    pub files: Vec<FileVisual>,
    pub contributors: Vec<ContributorVisual>,
    pub actions: Vec<ActionVisual>,
    pub branches: Vec<BranchVisual>,
    pub labels: Vec<LabelVisual>,
}

impl SceneSnapshot {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            tick: 0,
            repository_time: RationalTime::default(),
            generation: 0,
            directories: Vec::new(),
            files: Vec::new(),
            contributors: Vec::new(),
            actions: Vec::new(),
            branches: Vec::new(),
            labels: Vec::new(),
        }
    }
    /// Computes finite overview bounds for every drawable in the snapshot.
    ///
    /// Radii, action trails (including their live targets), branch endpoints,
    /// and the estimated extent of labels are included so an overview camera
    /// never clips a visual that is present in the snapshot.
    #[must_use]
    pub fn bounds(&self) -> Option<SceneBounds> {
        let mut bounds = None;
        for directory in &self.directories {
            include_bounds(&mut bounds, directory.position, directory.radius);
        }
        for file in &self.files {
            include_bounds(&mut bounds, file.position, file.radius);
        }
        for contributor in &self.contributors {
            let energy = if contributor.energy.is_finite() {
                contributor.energy.clamp(0.0, 1.0)
            } else {
                0.0
            };
            include_bounds(&mut bounds, contributor.position, 0.018 + energy * 0.018);
        }
        for action in &self.actions {
            include_bounds(&mut bounds, action.position, 0.025);
            include_bounds(&mut bounds, action.target, 0.025);
        }
        for branch in &self.branches {
            include_bounds(&mut bounds, branch.start, 0.01);
            include_bounds(&mut bounds, branch.end, 0.01);
        }
        for label in &self.labels {
            let glyph_count = label.text.chars().count().min(256) as f32;
            let width = (0.018 * glyph_count).clamp(0.036, 0.9);
            let priority = f32::from(label.priority.min(4));
            let height = 0.025 * (1.0 + 0.12 * priority);
            include_bounds_rect(
                &mut bounds,
                label.position,
                Vec2::new(-width * 0.5, -height * 0.15),
                Vec2::new(width * 0.5, height * 1.05),
            );
        }
        bounds
    }
}

fn include_bounds(bounds: &mut Option<SceneBounds>, center: Vec2, radius: f32) {
    if !center.x.is_finite() || !center.y.is_finite() {
        return;
    }
    let radius = if radius.is_finite() {
        radius.abs()
    } else {
        0.0
    };
    let min = Vec2::new(center.x - radius, center.y - radius);
    let max = Vec2::new(center.x + radius, center.y + radius);
    match bounds {
        Some(bounds) => {
            bounds.min.x = bounds.min.x.min(min.x);
            bounds.min.y = bounds.min.y.min(min.y);
            bounds.max.x = bounds.max.x.max(max.x);
            bounds.max.y = bounds.max.y.max(max.y);
        }
        None => *bounds = Some(SceneBounds::new(min, max)),
    }
}
fn include_bounds_rect(
    bounds: &mut Option<SceneBounds>,
    center: Vec2,
    min_offset: Vec2,
    max_offset: Vec2,
) {
    if !center.x.is_finite() || !center.y.is_finite() {
        return;
    }
    let min = center.add(min_offset);
    let max = center.add(max_offset);
    if !min.x.is_finite() || !min.y.is_finite() || !max.x.is_finite() || !max.y.is_finite() {
        return;
    }
    match bounds {
        Some(bounds) => {
            bounds.min.x = bounds.min.x.min(min.x);
            bounds.min.y = bounds.min.y.min(min.y);
            bounds.max.x = bounds.max.x.max(max.x);
            bounds.max.y = bounds.max.y.max(max.y);
        }
        None => *bounds = Some(SceneBounds::new(min, max)),
    }
}

/// A serial reference force solver.  It intentionally has no shared state or
/// parallel reduction: this is the behavioral reference for later measured
/// optimizations.
#[derive(Clone, Debug)]
pub struct SerialForceReference {
    repulsion: f32,
    damping: f32,
}

impl Default for SerialForceReference {
    fn default() -> Self {
        Self {
            repulsion: 0.045,
            damping: 0.84,
        }
    }
}

impl SerialForceReference {
    #[must_use]
    pub fn new(repulsion: f32, damping: f32) -> Self {
        Self {
            repulsion: if repulsion.is_finite() {
                repulsion.max(0.0)
            } else {
                0.0
            },
            damping: if damping.is_finite() {
                damping.clamp(0.0, 1.0)
            } else {
                0.84
            },
        }
    }

    /// Applies one deterministic, bounded relaxation toward stable anchors.
    ///
    /// The old solver accumulated an inverse-distance force forever.  This
    /// variant uses a bounded overlap force and a spring toward each supplied
    /// anchor, so idle replay converges instead of expanding the scene.
    pub fn relax_toward(&self, positions: &mut [Vec2], anchors: &[Vec2]) {
        let count = positions.len().min(anchors.len());
        if count == 0 {
            return;
        }
        let mut delta = vec![Vec2::default(); count];
        const COLLISION_DISTANCE: f32 = 0.42;
        for i in 0..count {
            for j in (i + 1)..count {
                let offset = positions[i].sub(positions[j]);
                let distance = offset.length().max(0.001);
                if distance >= COLLISION_DISTANCE {
                    continue;
                }
                let overlap = (COLLISION_DISTANCE - distance) / COLLISION_DISTANCE;
                let force = (self.repulsion * overlap).min(0.06);
                let direction = offset.scale(1.0 / distance);
                let change = direction.scale(force);
                delta[i] = delta[i].add(change);
                delta[j] = delta[j].sub(change);
            }
        }
        let movement = (0.16 * self.damping).clamp(0.02, 0.16);
        for index in 0..count {
            let spring = anchors[index].sub(positions[index]).scale(movement);
            let change = spring.add(delta[index]).clamped_radius(MAX_LAYOUT_SPEED);
            positions[index] = positions[index]
                .add(change)
                .clamped_radius(MAX_LAYOUT_RADIUS);
        }
    }

    /// Applies one deterministic bounded relaxation toward the origin.
    ///
    /// This compatibility entry point remains useful to callers that do not
    /// track hierarchy anchors; unlike the former implementation it cannot
    /// drift without bound.
    pub fn relax(&self, positions: &mut [Vec2]) {
        let anchors = vec![Vec2::default(); positions.len()];
        self.relax_toward(positions, &anchors);
    }
}

/// Bounds the amount of mutable replay work performed by one interactive pump.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkBudget {
    pub max_ticks: u64,
    pub max_events: usize,
}

impl WorkBudget {
    #[must_use]
    pub const fn new(max_ticks: u64, max_events: usize) -> Self {
        Self {
            max_ticks,
            max_events,
        }
    }
}

impl Default for WorkBudget {
    fn default() -> Self {
        Self {
            max_ticks: 8,
            max_events: 256,
        }
    }
}

/// Result of one bounded replay pump.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PumpResult {
    pub ticks: u64,
    pub events: usize,
    pub complete: bool,
    pub cancelled: bool,
    pub snapshot_tick: u64,
}

/// Cancellation shared with a caller without sharing mutable replay state.
#[derive(Clone, Debug, Default)]
pub struct Cancellation {
    cancelled: bool,
}

impl Cancellation {
    #[must_use]
    pub const fn new() -> Self {
        Self { cancelled: false }
    }

    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    pub fn reset(&mut self) {
        self.cancelled = false;
    }

    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

/// A caller-visible checkpoint token containing complete replay state.
#[derive(Clone, Debug)]
pub struct ReplayCheckpoint {
    identity: u128,
    generation: u64,
    state: ReplayState,
    clock: PlaybackClock,
    snapshot: SceneSnapshot,
}

impl ReplayCheckpoint {
    #[must_use]
    pub fn identity(&self) -> u128 {
        self.identity
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn tick(&self) -> u64 {
        self.state.tick
    }
}

/// Errors raised before a replay state can be published.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplayError {
    InvalidConfig(&'static str),
    CheckpointIdentityMismatch,
    CheckpointGenerationMismatch,
    /// The raw rational time cannot be normalized to this type's
    /// `i128`/positive-`i128` representation.
    RationalTimeOutOfRange,
    /// The requested repository time cannot be represented by a canonical
    /// `u64` tick.
    RepositoryTimeOverflow,
    World(WorldError),
    Cancelled,
    WorkBudgetExhausted,
}

impl fmt::Display for ReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(formatter, "invalid replay config: {message}"),
            Self::CheckpointIdentityMismatch => formatter.write_str("checkpoint identity mismatch"),
            Self::CheckpointGenerationMismatch => {
                formatter.write_str("checkpoint generation mismatch")
            }
            Self::RationalTimeOutOfRange => {
                formatter.write_str("rational time is outside the representable range")
            }
            Self::RepositoryTimeOverflow => {
                formatter.write_str("repository time exceeds the representable tick range")
            }
            Self::World(error) => write!(formatter, "world transition failed: {error}"),
            Self::Cancelled => formatter.write_str("replay cancelled"),
            Self::WorkBudgetExhausted => formatter.write_str("replay work budget exhausted"),
        }
    }
}

impl std::error::Error for ReplayError {}

#[derive(Clone, Copy, Debug, Default)]
struct LayoutState {
    position: Vec2,
    anchor: Vec2,
    velocity: Vec2,
    fresh: bool,
}

#[derive(Clone, Copy, Debug)]
struct LayoutPoint {
    id: HierarchyNodeId,
    parent: DirId,
    position: Vec2,
    anchor: Vec2,
    velocity: Vec2,
    fresh: bool,
}

#[derive(Clone, Copy, Debug)]
struct LayoutGroup {
    start: usize,
    end: usize,
    pair_work: u128,
}

#[derive(Clone, Debug)]
struct FileState {
    incarnation: u64,
    position: Vec2,
    anchor: Vec2,
    velocity: Vec2,
    fresh: bool,
    visible: bool,
    fade_remaining: u64,
    fade_start_tick: Option<u64>,
    last_activity_tick: u64,
    color: Rgb8,
}

#[derive(Clone, Debug)]
struct ContributorState {
    position: Vec2,
    energy: f32,
    last_action_tick: u64,
    color: Rgb8,
}

#[derive(Clone, Debug)]
struct ActionState {
    contributor: ContributorId,
    path_id: PathId,
    file_id: Option<FileId>,
    action: Action,
    started_tick: u64,
    origin: Vec2,
    target: Vec2,
    color: Rgb8,
}

#[derive(Clone, Debug)]
struct PendingTick {
    target_tick: u64,
    target_time: RationalTime,
}

#[derive(Clone, Debug)]
struct ReplayState {
    tick: u64,
    next_event: usize,
    world: World,
    directories: BTreeMap<DirId, LayoutState>,
    files: BTreeMap<FileId, FileState>,
    contributors: BTreeMap<ContributorId, ContributorState>,
    actions: Vec<ActionState>,
    published_tick: u64,
    wall_remainder_nanos: u128,
    idle_ticks: u64,
    pending_tick: Option<PendingTick>,
}
impl Default for ReplayState {
    fn default() -> Self {
        Self {
            tick: 0,
            next_event: 0,
            world: World::new(),
            directories: BTreeMap::new(),
            files: BTreeMap::new(),
            contributors: BTreeMap::new(),
            actions: Vec::new(),
            published_tick: 0,
            wall_remainder_nanos: 0,
            idle_ticks: 0,
            pending_tick: None,
        }
    }
}

/// Immutable three-buffer latest-wins snapshot exchange.
///
/// Interactive consumers may discard old snapshots, but `ReplaySession`
/// remains responsible for applying every event before a snapshot is built.
#[derive(Clone, Debug)]
pub struct SnapshotBufferPool {
    buffers: VecDeque<Arc<SceneSnapshot>>,
    capacity: usize,
    published: u64,
    discarded: u64,
}

impl Default for SnapshotBufferPool {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotBufferPool {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffers: VecDeque::with_capacity(MAX_SNAPSHOT_BUFFERS),
            capacity: MAX_SNAPSHOT_BUFFERS,
            published: 0,
            discarded: 0,
        }
    }

    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buffers: VecDeque::with_capacity(capacity.clamp(1, MAX_SNAPSHOT_BUFFERS)),
            capacity: capacity.clamp(1, MAX_SNAPSHOT_BUFFERS),
            published: 0,
            discarded: 0,
        }
    }

    pub fn publish(&mut self, snapshot: Arc<SceneSnapshot>) {
        self.published = self.published.saturating_add(1);
        self.buffers.push_back(snapshot);
        while self.buffers.len() > self.capacity {
            self.buffers.pop_front();
            self.discarded = self.discarded.saturating_add(1);
        }
    }

    /// Takes the newest snapshot and discards older interactive snapshots.
    pub fn acquire_latest(&mut self) -> Option<Arc<SceneSnapshot>> {
        let latest = self.buffers.pop_back();
        self.discarded = self.discarded.saturating_add(self.buffers.len() as u64);
        self.buffers.clear();
        latest
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.buffers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    #[must_use]
    pub const fn published(&self) -> u64 {
        self.published
    }

    #[must_use]
    pub const fn discarded(&self) -> u64 {
        self.discarded
    }
}

/// Short compatibility alias for callers that refer to the exchange as a
/// snapshot pool.
pub type SnapshotPool = SnapshotBufferPool;

/// Short compatibility alias for bounded replay work.
pub type ReplayWorkBudget = WorkBudget;

/// Deterministic fixed-step replay owner.
#[derive(Clone, Debug)]
pub struct ReplaySession<H: HistorySource> {
    history: H,
    config: ReplayConfig,
    execution_mode: ExecutionMode,
    parallel_pool: Option<Arc<ThreadPool>>,
    parallel_scratch: Vec<Vec2>,
    parallel_groups: Vec<LayoutGroup>,
    state: ReplayState,
    identity: u128,
    generation: u64,
    start_timestamp: i64,
    clock: PlaybackClock,
    snapshot: Arc<SceneSnapshot>,
    snapshots: SnapshotBufferPool,
    checkpoints: VecDeque<ReplayCheckpoint>,
    cancellation: Cancellation,
    force_reference: SerialForceReference,
}

impl<H: HistorySource> ReplaySession<H> {
    /// Creates a fresh session at the origin of the supplied finite history.
    pub fn new(history: H, config: ReplayConfig) -> Result<Self, ReplayError> {
        Self::new_with_execution(history, config, ExecutionMode::Serial)
    }

    /// Creates a fresh session with an explicit execution strategy.
    pub fn new_with_execution(
        history: H,
        config: ReplayConfig,
        execution_mode: ExecutionMode,
    ) -> Result<Self, ReplayError> {
        validate_config(&config)?;
        let start_timestamp = history.events().first().map(Event::timestamp).unwrap_or(0);
        let clock = PlaybackClock::from_config(&config, start_timestamp)
            .map_err(|_| ReplayError::InvalidConfig("clock configuration rejected"))?;
        let parallel_pool = match execution_mode {
            ExecutionMode::Serial => None,
            ExecutionMode::Parallel { threads } => Some(Arc::new(
                ThreadPoolBuilder::new()
                    .num_threads(threads.get())
                    .build()
                    .map_err(|_| ReplayError::InvalidConfig("parallel execution pool rejected"))?,
            )),
        };
        let mut session = Self {
            identity: replay_identity(&history, &config),
            history,
            config,
            execution_mode,
            parallel_pool,
            parallel_scratch: Vec::new(),
            parallel_groups: Vec::new(),
            state: ReplayState::default(),
            generation: 0,
            start_timestamp,
            clock,
            snapshot: Arc::new(SceneSnapshot::empty()),
            snapshots: SnapshotBufferPool::new(),
            checkpoints: VecDeque::new(),
            cancellation: Cancellation::new(),
            force_reference: SerialForceReference::default(),
        };
        session.reset_state()?;
        Ok(session)
    }

    /// Compatibility constructor for callers that prefer an explicit origin.
    pub fn from_origin(history: H, config: ReplayConfig) -> Result<Self, ReplayError> {
        Self::new(history, config)
    }

    #[must_use]
    pub fn history(&self) -> &H {
        &self.history
    }

    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        self.history.catalog()
    }

    #[must_use]
    pub fn config(&self) -> &ReplayConfig {
        &self.config
    }

    #[must_use]
    pub const fn execution_mode(&self) -> ExecutionMode {
        self.execution_mode
    }

    #[must_use]
    pub const fn configured_threads(&self) -> usize {
        self.execution_mode.configured_threads()
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn tick_id(&self) -> Tick {
        Tick::new(self.state.tick).unwrap_or(Tick::ZERO)
    }

    #[must_use]
    pub fn generation_id(&self) -> Generation {
        Generation::new(self.generation).unwrap_or(Generation::ZERO)
    }

    pub fn repository_time(&self) -> RationalTime {
        RationalTime::from(self.clock.repository_rational())
    }

    #[must_use]
    pub fn repository_rational(&self) -> Rational {
        Rational::new(
            self.repository_time().numerator,
            self.repository_time().denominator,
        )
        .unwrap_or(Rational::ZERO)
    }

    #[must_use]
    pub fn is_at_end(&self) -> bool {
        self.state.pending_tick.is_none()
            && self.state.next_event >= self.history.len()
            && self.state.actions.is_empty()
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub fn cancel(&mut self) {
        self.cancellation.cancel();
    }

    pub fn clear_cancellation(&mut self) {
        self.cancellation.reset();
    }

    /// Returns the latest fully prepared snapshot.  Partial wall-clock ticks
    /// never replace this value.
    #[must_use]
    pub fn snapshot(&self) -> Arc<SceneSnapshot> {
        Arc::clone(&self.snapshot)
    }

    #[must_use]
    pub fn snapshot_pool(&self) -> &SnapshotBufferPool {
        &self.snapshots
    }

    /// Advances exactly one canonical 120 Hz tick.
    pub fn step(&mut self) -> Result<Arc<SceneSnapshot>, ReplayError> {
        self.advance_ticks(1)
    }

    /// Advances a fixed number of ticks independent of wall-clock cadence.
    pub fn advance_ticks(&mut self, ticks: u64) -> Result<Arc<SceneSnapshot>, ReplayError> {
        if self.cancellation.is_cancelled() {
            return Err(ReplayError::Cancelled);
        }
        for _ in 0..ticks {
            self.advance_one_tick(usize::MAX)?;
        }
        Ok(self.snapshot())
    }

    /// Alias for [`Self::advance_ticks`].
    pub fn advance(&mut self, ticks: u64) -> Result<Arc<SceneSnapshot>, ReplayError> {
        self.advance_ticks(ticks)
    }

    /// Converts wall-clock duration to whole canonical ticks.  The fractional
    /// remainder is retained but no snapshot is published for it.
    pub fn advance_wall(&mut self, elapsed: Duration) -> Result<Arc<SceneSnapshot>, ReplayError> {
        if self.cancellation.is_cancelled() {
            return Err(ReplayError::Cancelled);
        }
        // Keep the remainder in nanoseconds×Hz so repeated cadence choices
        // cannot lose a fractional tick through integer division.
        let scaled_nanos = self
            .state
            .wall_remainder_nanos
            .saturating_add(elapsed.as_nanos().saturating_mul(SIMULATION_HZ as u128));
        let ticks = scaled_nanos / 1_000_000_000;
        self.state.wall_remainder_nanos = scaled_nanos % 1_000_000_000;
        self.advance_ticks(ticks as u64)
    }

    /// Performs bounded replay work while retaining any in-progress tick
    /// transaction across calls.
    pub fn pump(&mut self, budget: WorkBudget) -> Result<PumpResult, ReplayError> {
        if self.cancellation.is_cancelled() {
            return Ok(PumpResult {
                ticks: 0,
                events: 0,
                complete: false,
                cancelled: true,
                snapshot_tick: self.state.published_tick,
            });
        }
        if budget.max_ticks == 0 || budget.max_events == 0 {
            return Ok(PumpResult {
                ticks: 0,
                events: 0,
                complete: self.is_at_end(),
                cancelled: false,
                snapshot_tick: self.state.published_tick,
            });
        }
        let mut ticks = 0;
        let mut events = 0;
        while ticks < budget.max_ticks {
            if self.cancellation.is_cancelled() {
                break;
            }
            let before = self.state.next_event;
            let completed = self.advance_one_tick(budget.max_events.saturating_sub(events))?;
            let applied = self.state.next_event.saturating_sub(before);
            events = events.saturating_add(applied);
            if !completed {
                // The canonical tick remains in progress.  Its events have
                // been staged in replay state, but no clock/tick increment or
                // snapshot publication is allowed until the due set is done.
                break;
            }
            ticks = ticks.saturating_add(1);
            if events >= budget.max_events {
                break;
            }
        }
        Ok(PumpResult {
            ticks,
            events,
            complete: self.is_at_end(),
            cancelled: self.cancellation.is_cancelled(),
            snapshot_tick: self.state.published_tick,
        })
    }

    /// Seeks from the origin, restoring every mutable field before replaying
    /// forward to the floor-snapped target tick.
    pub fn seek_tick(&mut self, target_tick: u64) -> Result<Arc<SceneSnapshot>, ReplayError> {
        if self.cancellation.is_cancelled() {
            return Err(ReplayError::Cancelled);
        }
        self.generation = self.generation.saturating_add(1);
        self.reset_state()?;
        for _ in 0..target_tick {
            self.advance_one_tick(usize::MAX)?;
        }
        Ok(self.snapshot())
    }

    /// Seeks to repository time using floor semantics at 120 Hz.  Targets
    /// before the history origin remain at tick zero.
    pub fn seek_repository_time<T>(&mut self, target: T) -> Result<Arc<SceneSnapshot>, ReplayError>
    where
        T: Into<RationalTime>,
    {
        let ticks = self.ticks_for_repository_time(target.into())?;
        self.seek_tick(ticks)
    }

    /// Alias used by export schedulers; accepts either this crate's rational
    /// value or the core rational value.
    pub fn seek<T>(&mut self, target: T) -> Result<Arc<SceneSnapshot>, ReplayError>
    where
        T: Into<RationalTime>,
    {
        self.seek_repository_time(target)
    }

    /// Captures a complete, identity-keyed replay checkpoint.
    pub fn checkpoint(&mut self) -> ReplayCheckpoint {
        let checkpoint = ReplayCheckpoint {
            identity: self.identity,
            generation: self.generation,
            state: self.state.clone(),
            clock: self.clock.clone(),
            snapshot: (*self.snapshot).clone(),
        };
        self.checkpoints.push_back(checkpoint.clone());
        while self.checkpoints.len() > MAX_CHECKPOINTS {
            self.checkpoints.pop_front();
        }
        checkpoint
    }

    /// Restores a checkpoint only if it belongs to this replay identity and
    /// generation.  Restoring never changes the source or drops events.
    pub fn restore_checkpoint(
        &mut self,
        checkpoint: &ReplayCheckpoint,
    ) -> Result<Arc<SceneSnapshot>, ReplayError> {
        if checkpoint.identity != self.identity {
            return Err(ReplayError::CheckpointIdentityMismatch);
        }
        if checkpoint.generation != self.generation {
            return Err(ReplayError::CheckpointGenerationMismatch);
        }
        self.state = checkpoint.state.clone();
        self.clock = checkpoint.clock.clone();
        self.snapshot = Arc::new(checkpoint.snapshot.clone());
        self.snapshots.publish(Arc::clone(&self.snapshot));
        Ok(self.snapshot())
    }

    /// Starts a new replay generation at origin, invalidating old checkpoints.
    pub fn restart(&mut self) {
        self.generation = self.generation.saturating_add(1);
        // Construction validates the configuration and the origin event set;
        // replaying that same immutable prefix cannot fail here.
        self.reset_state()
            .expect("validated replay origin must remain applicable");
        self.checkpoints.clear();
    }

    #[must_use]
    pub fn checkpoint_count(&self) -> usize {
        self.checkpoints.len()
    }

    fn reset_state(&mut self) -> Result<(), ReplayError> {
        self.state = ReplayState::default();
        self.clock = PlaybackClock::from_config(&self.config, self.start_timestamp)
            .map_err(|_| ReplayError::InvalidConfig("clock configuration rejected"))?;
        self.cancellation.reset();
        self.apply_origin_events()?;
        self.rebuild_snapshot()?;
        Ok(())
    }

    fn apply_origin_events(&mut self) -> Result<(), ReplayError> {
        let origin = self.repository_time_at(0)?;
        let timestamp = origin.numerator.div_euclid(origin.denominator);
        while self.state.next_event < self.history.len() {
            let event = self.history.events()[self.state.next_event].clone();
            if i128::from(event.timestamp()) > timestamp {
                break;
            }
            self.apply_event(&event)?;
            self.state.next_event += 1;
        }
        Ok(())
    }

    fn advance_one_tick(&mut self, event_budget: usize) -> Result<bool, ReplayError> {
        if self.cancellation.is_cancelled() {
            return Err(ReplayError::Cancelled);
        }
        let pending = match self.state.pending_tick.clone() {
            Some(pending) => pending,
            None => {
                let target_tick = self
                    .state
                    .tick
                    .checked_add(1)
                    .ok_or(ReplayError::InvalidConfig("tick overflow"))?;
                let normal_target_time = self.repository_time_at(target_tick)?;
                let target_time = self.auto_skip_target(normal_target_time)?;
                PendingTick {
                    target_tick,
                    target_time,
                }
            }
        };
        self.state.pending_tick = Some(pending.clone());
        let timestamp = pending
            .target_time
            .numerator
            .div_euclid(pending.target_time.denominator);
        let mut applied = 0usize;
        while self.state.next_event < self.history.len() {
            if applied >= event_budget {
                return Ok(false);
            }
            let event = self.history.events()[self.state.next_event].clone();
            if i128::from(event.timestamp()) > timestamp {
                break;
            }
            self.apply_event(&event)?;
            self.state.next_event += 1;
            applied += 1;
        }
        self.jump_clock_to(pending.target_time)?;
        self.clock
            .advance_ticks(1)
            .map_err(|_| ReplayError::InvalidConfig("clock overflow"))?;
        self.state.tick = pending.target_tick;
        self.state.pending_tick = None;
        self.integrate_state()?;
        self.rebuild_snapshot()?;
        Ok(true)
    }

    fn reconcile_layout(&mut self) {
        let root = self.state.world.root();
        let mut world_directories = self.state.world.directories();
        world_directories.retain(|directory| directory.id != root);
        world_directories.sort_by_key(|directory| (directory.depth(), directory.id));
        let mut active_directory_ids: Vec<DirId> = world_directories
            .iter()
            .map(|directory| directory.id)
            .collect();
        active_directory_ids.sort_unstable();
        {
            let directory_layout = &mut self.state.directories;
            for directory in &world_directories {
                let parent_position = directory
                    .parent
                    .and_then(|parent| directory_layout.get(&parent))
                    .map_or(Vec2::default(), |layout| layout.position);
                let anchor = parent_position
                    .add(directory_anchor(
                        self.config.seed,
                        self.config.algorithm_version,
                        directory.id,
                        directory.parent.unwrap_or(root),
                        directory.depth(),
                    ))
                    .clamped_radius(MAX_LAYOUT_RADIUS);
                let layout = directory_layout.entry(directory.id).or_insert(LayoutState {
                    position: anchor,
                    anchor,
                    velocity: Vec2::default(),
                    fresh: true,
                });
                if !layout.position.x.is_finite() || !layout.position.y.is_finite() {
                    layout.position = anchor;
                    layout.fresh = true;
                    layout.velocity = Vec2::default();
                }
                layout.anchor = anchor;
                if !layout.velocity.x.is_finite() || !layout.velocity.y.is_finite() {
                    layout.velocity = Vec2::default();
                }
            }
            directory_layout.retain(|id, _| active_directory_ids.binary_search(id).is_ok());
        }

        let directory_layout = &self.state.directories;
        let world = &self.state.world;
        for (file_id, file) in &mut self.state.files {
            let Some(node) = world.file(*file_id) else {
                continue;
            };
            let parent_position = directory_layout
                .get(&node.parent)
                .map_or(Vec2::default(), |layout| layout.position);
            let anchor = parent_position
                .add(file_anchor(
                    self.config.seed,
                    self.config.algorithm_version,
                    *file_id,
                    node.parent,
                    node.path_id,
                ))
                .clamped_radius(MAX_LAYOUT_RADIUS);
            file.anchor = anchor;
            if !file.position.x.is_finite() || !file.position.y.is_finite() {
                file.fresh = true;
                file.position = anchor;
                file.velocity = Vec2::default();
            }
            if !file.velocity.x.is_finite() || !file.velocity.y.is_finite() {
                file.velocity = Vec2::default();
            }
        }
    }

    fn relax_layout_points(&mut self, points: &mut [LayoutPoint]) {
        let pool = if self.configured_threads() > 1 {
            self.parallel_pool.as_deref()
        } else {
            None
        };
        relax_layout_points_grouped(
            points,
            &self.force_reference,
            pool,
            &mut self.parallel_scratch,
            &mut self.parallel_groups,
        );
    }

    fn layout_step(&mut self) {
        self.reconcile_layout();

        let mut directories = self
            .state
            .directories
            .iter()
            .map(|(id, layout)| LayoutPoint {
                id: HierarchyNodeId::Directory(*id),
                parent: self
                    .state
                    .world
                    .directory(*id)
                    .and_then(|directory| directory.parent)
                    .unwrap_or(self.state.world.root()),
                position: layout.position,
                anchor: layout.anchor,
                velocity: layout.velocity,
                fresh: layout.fresh,
            })
            .collect::<Vec<_>>();
        self.relax_layout_points(&mut directories);
        for point in directories {
            let HierarchyNodeId::Directory(id) = point.id else {
                continue;
            };
            if let Some(layout) = self.state.directories.get_mut(&id) {
                layout.anchor = point.anchor;
                layout.position = point.position;
                layout.fresh = false;
                layout.velocity = point.velocity;
            }
        }

        // Directory movement changes the desired positions of child files.
        // Refresh those anchors before solving all sibling kinds together so a
        // file and directory sharing one parent cannot remain colliding.
        self.reconcile_layout();
        let mut points = self
            .state
            .directories
            .iter()
            .map(|(id, layout)| LayoutPoint {
                id: HierarchyNodeId::Directory(*id),
                parent: self
                    .state
                    .world
                    .directory(*id)
                    .and_then(|directory| directory.parent)
                    .unwrap_or(self.state.world.root()),
                position: layout.position,
                anchor: layout.anchor,
                velocity: layout.velocity,
                fresh: layout.fresh,
            })
            .collect::<Vec<_>>();
        points.extend(
            self.state
                .files
                .iter()
                .filter(|(_, file)| file.visible)
                .filter_map(|(id, file)| {
                    let parent = self.state.world.file(*id)?.parent;
                    Some(LayoutPoint {
                        id: HierarchyNodeId::File(*id),
                        parent,
                        position: file.position,
                        anchor: file.anchor,
                        velocity: file.velocity,
                        fresh: file.fresh,
                    })
                }),
        );
        self.relax_layout_points(&mut points);
        for point in points {
            match point.id {
                HierarchyNodeId::Directory(id) => {
                    if let Some(layout) = self.state.directories.get_mut(&id) {
                        layout.anchor = point.anchor;
                        layout.fresh = false;
                        layout.position = point.position;
                        layout.velocity = point.velocity;
                    }
                }
                HierarchyNodeId::File(id) => {
                    if let Some(file) = self.state.files.get_mut(&id) {
                        file.anchor = point.anchor;
                        file.fresh = false;
                        file.position = point.position;
                        file.velocity = point.velocity;
                    }
                }
            }
        }
    }

    fn file_color(&self, path_id: PathId) -> Rgb8 {
        let key = self
            .catalog()
            .path(path_id)
            .map(|path| {
                let canonical = path.canonical();
                let basename = canonical.rsplit('/').next().unwrap_or(canonical);
                basename
                    .rsplit_once('.')
                    .filter(|(_, extension)| !extension.is_empty())
                    .map_or(canonical, |(_, extension)| extension)
            })
            .unwrap_or("path");
        let hash = stable_hash_numbers(&[
            self.config.seed,
            u64::from(self.config.algorithm_version),
            stable_hash(key),
        ]);
        FILE_PALETTE[(hash as usize) % FILE_PALETTE.len()]
    }

    fn contributor_color(&self, contributor_id: ContributorId) -> Rgb8 {
        let key_hash = self
            .catalog()
            .contributor(contributor_id)
            .map(stable_hash)
            .unwrap_or_else(|| stable_hash_numbers(&[contributor_id.as_u64()]));
        let hash = stable_hash_numbers(&[
            self.config.seed,
            u64::from(self.config.algorithm_version),
            key_hash,
            0xC0_17_4B,
        ]);
        FILE_PALETTE[(hash as usize) % FILE_PALETTE.len()]
    }

    fn contributor_position(&self, contributor_id: ContributorId) -> Vec2 {
        contributor_anchor(
            self.config.seed,
            self.config.algorithm_version,
            contributor_id,
        )
    }

    fn apply_event(&mut self, event: &Event) -> Result<(), ReplayError> {
        let current_tick = self
            .state
            .pending_tick
            .as_ref()
            .map_or(self.state.tick, |pending| pending.target_tick);
        let path_id = event.path_id();
        let previous_file_id = self.state.world.active_file(path_id);
        let previous_target = previous_file_id
            .and_then(|file_id| self.state.files.get(&file_id))
            .map(|file| file.position);
        let contributor_position = self.state.contributors.get(&event.contributor).map_or_else(
            || self.contributor_position(event.contributor),
            |contributor| contributor.position,
        );
        let catalog = self.history.catalog();
        let delta = self
            .state
            .world
            .apply_event(event, catalog)
            .map_err(ReplayError::World)?;

        // Capture delete positions before changing visibility.  World keeps
        // dead FileNodes addressable, but their visual state is intentionally
        // faded and eventually removed from the live map.
        let deleted_targets = delta
            .deleted_files
            .iter()
            .filter_map(|file_id| {
                self.state
                    .files
                    .get(file_id)
                    .map(|file| (*file_id, file.position))
            })
            .collect::<Vec<_>>();
        for file_id in &delta.deleted_files {
            if let Some(file) = self.state.files.get_mut(file_id) {
                file.visible = false;
                file.fade_remaining = FILE_FADE_TICKS;
                file.fade_start_tick = Some(current_tick);
                if let Some(color) = event.color {
                    file.color = color;
                }
            }
        }
        for file_id in &delta.created_files {
            if let Some(node) = self.state.world.file(*file_id) {
                self.state.files.insert(
                    *file_id,
                    FileState {
                        incarnation: file_id.as_u64(),
                        position: Vec2::default(),
                        anchor: Vec2::default(),
                        velocity: Vec2::default(),
                        fresh: true,
                        visible: true,
                        fade_remaining: 0,
                        fade_start_tick: None,
                        last_activity_tick: current_tick,
                        color: node.color.unwrap_or_else(|| self.file_color(node.path_id)),
                    },
                );
            }
        }
        for file_id in &delta.modified_files {
            if let Some(node) = self.state.world.file(*file_id) {
                let color = node.color.unwrap_or_else(|| self.file_color(node.path_id));
                if let Some(file) = self.state.files.get_mut(file_id) {
                    file.visible = true;
                    file.fade_remaining = 0;
                    file.fade_start_tick = None;
                    file.last_activity_tick = current_tick;
                    file.color = color;
                } else {
                    self.state.files.insert(
                        *file_id,
                        FileState {
                            incarnation: file_id.as_u64(),
                            position: Vec2::default(),
                            fresh: true,
                            anchor: Vec2::default(),
                            velocity: Vec2::default(),
                            visible: true,
                            fade_remaining: 0,
                            fade_start_tick: None,
                            last_activity_tick: current_tick,
                            color,
                        },
                    );
                }
            }
        }

        self.reconcile_layout();
        for file_id in delta.created_files.iter().chain(&delta.modified_files) {
            if let Some(file) = self.state.files.get_mut(file_id)
                && file.fresh
            {
                file.position = file.anchor;
                file.velocity = Vec2::default();
            }
        }

        // Resolve the target using the post-transition live file identity.
        // Deletes bind to their exact fading incarnation; absent deletes and
        // directory activities intentionally remain unbound.
        let target_file_id = if matches!(event.target, EventTarget::File(_)) {
            match event.action {
                Action::Delete => delta.deleted_files.last().copied(),
                Action::Add | Action::Modify => self.state.world.active_file(path_id),
            }
        } else {
            None
        };
        let target_position = target_file_id
            .and_then(|file_id| self.state.files.get(&file_id))
            .map(|file| file.position)
            .or_else(|| deleted_targets.last().map(|(_, position)| *position))
            .or(previous_target)
            .unwrap_or_else(|| self.position_for_path(path_id));

        let contributor_color = self.contributor_color(event.contributor);
        let contributor =
            self.state
                .contributors
                .entry(event.contributor)
                .or_insert(ContributorState {
                    position: contributor_position,
                    energy: 0.0,
                    last_action_tick: current_tick,
                    color: contributor_color,
                });
        if let Some(color) = event.color {
            contributor.color = color;
        }
        let event_color = event.color.unwrap_or_else(|| action_color(event.action));
        self.state.actions.push(ActionState {
            contributor: event.contributor,
            path_id,
            file_id: target_file_id,
            action: event.action,
            started_tick: current_tick,
            origin: contributor_position,
            target: target_position,
            color: event_color,
        });
        Ok(())
    }

    fn integrate_state(&mut self) -> Result<(), ReplayError> {
        let file_idle_ticks = self
            .config
            .file_idle_ticks()
            .map_err(|_| ReplayError::InvalidConfig("file idle configuration rejected"))?;
        let current_tick = self.state.tick;
        for file in self.state.files.values_mut() {
            if !file.visible {
                if let Some(start_tick) = file.fade_start_tick {
                    let age = current_tick.saturating_sub(start_tick);
                    file.fade_remaining = FILE_FADE_TICKS.saturating_sub(age);
                }
            } else if let Some(idle_ticks) = file_idle_ticks
                && current_tick.saturating_sub(file.last_activity_tick) > idle_ticks
            {
                file.visible = false;
                file.fade_start_tick = Some(current_tick);
                file.fade_remaining = FILE_FADE_TICKS;
            }
        }
        self.state
            .files
            .retain(|_, file| file.visible || file.fade_remaining > 0);
        for action in &self.state.actions {
            if let Some(contributor) = self.state.contributors.get_mut(&action.contributor) {
                contributor.energy = (contributor.energy + 0.08).min(1.0);
                contributor.last_action_tick = self.state.tick;
                let progress = ((self.state.tick.saturating_sub(action.started_tick)) as f32
                    / ACTION_TRAVEL_TICKS as f32)
                    .clamp(0.0, 1.0);
                contributor.position = lerp(action.origin, action.target, progress);
            }
        }
        self.state.actions.retain(|action| {
            current_tick.saturating_sub(action.started_tick) <= ACTION_LIFETIME_TICKS
        });
        self.state.idle_ticks = if self.state.actions.is_empty() {
            self.state.idle_ticks.saturating_add(1)
        } else {
            0
        };
        self.layout_step();

        // Keep beams attached only to the exact file incarnation captured by
        // their event.  A deleted target remains fixed while fading and never
        // jumps to a later recreation of the same path.
        for action in &mut self.state.actions {
            if let Some(file_id) = action.file_id
                && let Some(file) = self.state.files.get(&file_id)
            {
                action.target = file.position;
            }
        }
        Ok(())
    }

    fn rebuild_snapshot(&mut self) -> Result<(), ReplayError> {
        self.reconcile_layout();
        let root = self.state.world.root();
        let mut directories_from_world = self.state.world.directories();
        directories_from_world.retain(|directory| directory.id != root);
        directories_from_world.sort_by_key(|directory| (directory.depth(), directory.id));
        let mut directory_positions = BTreeMap::new();
        let mut directories = Vec::with_capacity(directories_from_world.len());
        for directory in &directories_from_world {
            let position = self
                .state
                .directories
                .get(&directory.id)
                .map_or(Vec2::default(), |layout| layout.position);
            directory_positions.insert(directory.id, position);
            directories.push(DirectoryVisual {
                id: directory.id,
                position,
                radius: 0.14,
                opacity: 1.0,
                color: DIRECTORY_COLOR,
            });
        }

        let mut files: Vec<FileVisual> = self
            .state
            .files
            .iter()
            .filter_map(|(file_id, file)| {
                let node = self.state.world.file(*file_id)?;
                let active = file.visible && node.alive;
                Some(FileVisual {
                    path_id: node.path_id,
                    file_id: *file_id,
                    incarnation: file.incarnation,
                    position: file.position,
                    radius: 0.08,
                    opacity: if active {
                        1.0
                    } else {
                        file.fade_remaining as f32 / FILE_FADE_TICKS as f32
                    },
                    active,
                    color: file.color,
                })
            })
            .collect();
        files.sort_by_key(|file| (file.path_id, file.incarnation));

        let contributors: Vec<ContributorVisual> = self
            .state
            .contributors
            .iter()
            .map(|(id, contributor)| ContributorVisual {
                id: *id,
                position: contributor.position,
                energy: contributor.energy,
                active: contributor.energy > 0.01,
                color: contributor.color,
            })
            .collect();
        let mut actions = Vec::with_capacity(self.state.actions.len());
        for action in &self.state.actions {
            let progress = ((self.state.tick.saturating_sub(action.started_tick)) as f32
                / ACTION_TRAVEL_TICKS as f32)
                .clamp(0.0, 1.0);
            actions.push(ActionVisual {
                contributor: action.contributor,
                path_id: action.path_id,
                action: action.action,
                position: lerp(action.origin, action.target, progress),
                target: action.target,
                progress,
                opacity: (1.0 - progress * 0.7).max(0.0),
                color: action.color,
            });
        }
        actions.sort_by(|left, right| {
            left.contributor
                .cmp(&right.contributor)
                .then(left.path_id.cmp(&right.path_id))
                .then_with(|| {
                    left.progress
                        .partial_cmp(&right.progress)
                        .unwrap_or(Ordering::Equal)
                })
        });

        let mut branches = Vec::with_capacity(directories.len() + files.len());
        for (directory, visual) in directories_from_world.iter().zip(&directories) {
            let parent = directory
                .parent
                .filter(|parent| *parent != root && directory_positions.contains_key(parent));
            let start = parent
                .and_then(|id| directory_positions.get(&id).copied())
                .unwrap_or_default();
            branches.push(BranchVisual {
                parent,
                child: HierarchyNodeId::Directory(directory.id),
                start,
                end: visual.position,
                opacity: visual.opacity,
                color: visual.color,
            });
        }
        for (file_id, file) in &self.state.files {
            let Some(node) = self.state.world.file(*file_id) else {
                continue;
            };
            let parent = (node.parent != root && directory_positions.contains_key(&node.parent))
                .then_some(node.parent);
            let start = parent
                .and_then(|id| directory_positions.get(&id).copied())
                .unwrap_or_default();
            let active = file.visible && node.alive;
            let opacity = if active {
                1.0
            } else {
                file.fade_remaining as f32 / FILE_FADE_TICKS as f32
            };
            branches.push(BranchVisual {
                parent,
                child: HierarchyNodeId::File(*file_id),
                start,
                end: file.position,
                opacity,
                color: file.color,
            });
        }
        let labels = files
            .iter()
            .map(|file| LabelVisual {
                target: file.path_id,
                text: self.path_label(file.path_id),
                position: file.position,
                opacity_bits: file.opacity.to_bits(),
                priority: if file.active { 1 } else { 0 },
                visible: file.opacity > 0.0,
            })
            .collect();
        self.snapshot = Arc::new(SceneSnapshot {
            tick: self.state.tick,
            repository_time: self.repository_time_at(self.state.tick)?,
            generation: self.generation,
            directories,
            files,
            contributors,
            actions,
            branches,
            labels,
        });
        self.state.published_tick = self.state.tick;
        self.snapshots.publish(Arc::clone(&self.snapshot));
        Ok(())
    }

    fn path_label(&self, path_id: PathId) -> String {
        self.catalog()
            .path(path_id)
            .map(|path| path.canonical().to_owned())
            .unwrap_or_else(|| format!("{path_id:?}"))
    }

    fn auto_skip_target(
        &self,
        normal_target_time: RationalTime,
    ) -> Result<RationalTime, ReplayError> {
        let Some(threshold) = self.auto_skip_ticks() else {
            return Ok(normal_target_time);
        };
        if self.state.idle_ticks < threshold
            || !self.state.actions.is_empty()
            || self.state.next_event >= self.history.len()
        {
            return Ok(normal_target_time);
        }
        let next_event_time = RationalTime::new(
            i128::from(self.history.events()[self.state.next_event].timestamp()),
            1,
        )
        .reduced()?;
        let normal_target = normal_target_time.reduced()?;
        let normal_timestamp = normal_target
            .numerator
            .div_euclid(normal_target.denominator);
        if i128::from(self.history.events()[self.state.next_event].timestamp()) <= normal_timestamp
        {
            return Ok(normal_target_time);
        }
        Ok(next_event_time)
    }

    fn auto_skip_ticks(&self) -> Option<u64> {
        if self.config.auto_skip_seconds <= 0.0 {
            return None;
        }
        let seconds = Rational::from_f64(self.config.auto_skip_seconds).ok()?;
        let scaled = seconds
            .checked_mul(Rational::from_u64(SIMULATION_HZ))
            .ok()?;
        let ticks = scaled.ceil();
        u64::try_from(ticks).ok().filter(|ticks| *ticks > 0)
    }

    fn jump_clock_to(&mut self, target: RationalTime) -> Result<(), ReplayError> {
        let target = Rational::new(target.numerator, target.denominator)
            .map_err(|_| ReplayError::InvalidConfig("clock overflow"))?;
        let tick_fraction = Rational::new(1, SIMULATION_HZ as i128)
            .map_err(|_| ReplayError::InvalidConfig("clock overflow"))?;
        let per_tick = self
            .clock
            .repository_rate
            .checked_mul(tick_fraction)
            .map_err(|_| ReplayError::InvalidConfig("clock overflow"))?;
        let negative_tick = Rational::new(
            per_tick
                .numerator
                .checked_neg()
                .ok_or(ReplayError::InvalidConfig("clock overflow"))?,
            per_tick.denominator,
        )
        .map_err(|_| ReplayError::InvalidConfig("clock overflow"))?;
        let current = target
            .checked_add(negative_tick)
            .map_err(|_| ReplayError::InvalidConfig("clock overflow"))?;
        let start = Rational::new(i128::from(self.start_timestamp), 1)
            .map_err(|_| ReplayError::InvalidConfig("clock overflow"))?;
        let negative_start = Rational::new(
            start
                .numerator
                .checked_neg()
                .ok_or(ReplayError::InvalidConfig("clock overflow"))?,
            start.denominator,
        )
        .map_err(|_| ReplayError::InvalidConfig("clock overflow"))?;
        self.clock.repository_offset = current
            .checked_add(negative_start)
            .map_err(|_| ReplayError::InvalidConfig("clock overflow"))?;
        Ok(())
    }

    fn repository_time_at(&self, tick: u64) -> Result<RationalTime, ReplayError> {
        if tick == self.state.tick {
            return Ok(RationalTime::from(self.clock.repository_rational()));
        }
        let base = RationalTime::from(self.clock.repository_rational());
        let tick_delta = i128::from(tick)
            .checked_sub(i128::from(self.state.tick))
            .ok_or(ReplayError::RepositoryTimeOverflow)?;
        let rate = self.clock.repository_rate;
        let denominator = rate
            .denominator
            .checked_mul(i128::from(SIMULATION_HZ))
            .ok_or(ReplayError::RepositoryTimeOverflow)?;
        let elapsed_numerator = rate
            .numerator
            .checked_mul(tick_delta)
            .ok_or(ReplayError::RepositoryTimeOverflow)?;
        let elapsed = RationalTime::new(elapsed_numerator, denominator).reduced()?;
        base.checked_add(elapsed)
    }

    fn ticks_for_repository_time(&self, target: RationalTime) -> Result<u64, ReplayError> {
        let target = target.reduced()?;
        let rate = self.clock.repository_rate;
        if rate.numerator <= 0 || rate.denominator <= 0 {
            return Err(ReplayError::RepositoryTimeOverflow);
        }
        let origin_numerator = i128::from(self.start_timestamp)
            .checked_mul(target.denominator)
            .ok_or(ReplayError::RepositoryTimeOverflow)?;
        let elapsed = target
            .numerator
            .checked_sub(origin_numerator)
            .ok_or(ReplayError::RepositoryTimeOverflow)?;
        if elapsed <= 0 {
            return Ok(0);
        }

        let numerator = elapsed
            .checked_mul(rate.denominator)
            .and_then(|value| value.checked_mul(i128::from(SIMULATION_HZ)))
            .ok_or(ReplayError::RepositoryTimeOverflow)?;
        let denominator = target
            .denominator
            .checked_mul(rate.numerator)
            .ok_or(ReplayError::RepositoryTimeOverflow)?;
        if denominator <= 0 {
            return Err(ReplayError::RepositoryTimeOverflow);
        }
        let ticks = numerator / denominator;
        u64::try_from(ticks).map_err(|_| ReplayError::RepositoryTimeOverflow)
    }

    fn position_for_path(&self, path_id: PathId) -> Vec2 {
        let hash = stable_hash_numbers(&[
            self.config.seed,
            u64::from(self.config.algorithm_version),
            0x0FA1_1BAC,
            path_id.as_u64(),
        ]);
        hash_direction(hash).scale(0.75 + hash_unit(hash.rotate_left(7)) * 0.65)
    }
}

fn validate_config(config: &ReplayConfig) -> Result<(), ReplayError> {
    config
        .validate()
        .map_err(|_| ReplayError::InvalidConfig("core replay configuration rejected"))
}

fn replay_identity<H: HistorySource>(history: &H, config: &ReplayConfig) -> u128 {
    let mut hash = 0xcbf29ce484222325u128;
    mix_identity(&mut hash, ReplayConfig::VERSION as u128);
    mix_identity(&mut hash, history.len() as u128);
    mix_identity(&mut hash, config.seed as u128);
    mix_identity(&mut hash, config.algorithm_version as u128);
    mix_identity(&mut hash, config.seconds_per_day.to_bits() as u128);
    mix_identity(&mut hash, config.time_scale.to_bits() as u128);
    mix_identity(&mut hash, config.auto_skip_seconds.to_bits() as u128);
    mix_identity(&mut hash, config.realtime as u128);
    match config.file_idle_seconds {
        Some(value) => {
            mix_identity(&mut hash, 1);
            mix_identity(&mut hash, value.to_bits() as u128);
        }
        None => mix_identity(&mut hash, 0),
    }
    mix_identity(
        &mut hash,
        match config.camera_mode {
            gource_core::CameraMode::Overview => 0,
            gource_core::CameraMode::Track => 1,
        },
    );
    let limits = &config.limits;
    for value in [
        limits.record_bytes,
        limits.input_bytes,
        limits.path_bytes,
        limits.contributor_bytes,
        limits.path_components,
        limits.working_memory_bytes,
        limits.working_disk_bytes,
        limits.max_events,
        limits.max_paths,
        limits.max_contributors,
    ] {
        mix_identity(&mut hash, value as u128);
    }

    let catalog = history.catalog();
    mix_identity(&mut hash, catalog.version() as u128);
    mix_identity(&mut hash, catalog.path_count() as u128);
    for path in catalog.paths() {
        mix_identity(&mut hash, path.is_directory() as u128);
        mix_identity_bytes(&mut hash, path.as_bytes());
    }
    mix_identity(&mut hash, catalog.contributor_count() as u128);
    for contributor in catalog.contributors() {
        mix_identity_bytes(&mut hash, contributor.as_bytes());
    }

    for event in history.events() {
        mix_identity(&mut hash, event.version as u128);
        mix_identity(&mut hash, event.timestamp() as u128);
        mix_identity(&mut hash, event.source_sequence().get() as u128);
        mix_identity(&mut hash, event.generation.get() as u128);
        mix_identity(&mut hash, event.path_id().get() as u128);
        mix_identity(
            &mut hash,
            match event.target {
                EventTarget::File(_) => 0,
                EventTarget::Directory(_) => 1,
            },
        );
        mix_identity(&mut hash, event.contributor.get() as u128);
        mix_identity(&mut hash, event.action.as_byte() as u128);
        match event.color {
            Some(color) => {
                mix_identity(&mut hash, 1);
                mix_identity(
                    &mut hash,
                    u32::from_be_bytes([0, color.r, color.g, color.b]) as u128,
                );
            }
            None => mix_identity(&mut hash, 0),
        }
    }
    hash
}

fn mix_identity(hash: &mut u128, value: u128) {
    *hash = hash.wrapping_mul(0x100000001b3u128).wrapping_add(value);
}

fn mix_identity_bytes(hash: &mut u128, bytes: &[u8]) {
    mix_identity(hash, bytes.len() as u128);
    for byte in bytes {
        mix_identity(hash, u128::from(*byte));
    }
}

fn stable_hash(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn stable_hash_numbers(values: &[u64]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for value in values {
        for byte in value.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    hash
}

fn hash_unit(hash: u64) -> f32 {
    ((hash >> 16) as u32) as f32 / u32::MAX as f32
}

fn hash_direction(hash: u64) -> Vec2 {
    let angle = (hash as f64 / u64::MAX as f64 * std::f64::consts::TAU) as f32;
    Vec2::new(angle.cos(), angle.sin())
}

fn directory_anchor(
    seed: u64,
    algorithm_version: u32,
    id: DirId,
    parent: DirId,
    depth: usize,
) -> Vec2 {
    let hash = stable_hash_numbers(&[
        seed,
        u64::from(algorithm_version),
        0xD1_72_11,
        id.as_u64(),
        parent.as_u64(),
        depth as u64,
    ]);
    let depth_scale = (1.0 + depth as f32 * 0.04).sqrt();
    let radius = (0.78 / depth_scale).max(0.20) + hash_unit(hash.rotate_left(19)) * 0.16;
    hash_direction(hash).scale(radius)
}

fn file_anchor(
    seed: u64,
    algorithm_version: u32,
    id: FileId,
    parent: DirId,
    path_id: PathId,
) -> Vec2 {
    let hash = stable_hash_numbers(&[
        seed,
        u64::from(algorithm_version),
        0xF11E_5EED,
        id.as_u64(),
        parent.as_u64(),
        path_id.as_u64(),
    ]);
    hash_direction(hash).scale(0.40 + hash_unit(hash.rotate_left(23)) * 0.22)
}

fn contributor_anchor(seed: u64, algorithm_version: u32, id: ContributorId) -> Vec2 {
    let hash = stable_hash_numbers(&[seed, u64::from(algorithm_version), 0xC0_17_4B, id.as_u64()]);
    hash_direction(hash).scale(1.10 + hash_unit(hash.rotate_left(31)) * 0.28)
}

fn action_color(action: Action) -> Rgb8 {
    match action {
        Action::Add => ACTION_ADD_COLOR,
        Action::Modify => ACTION_MODIFY_COLOR,
        Action::Delete => ACTION_DELETE_COLOR,
    }
}

fn layout_identity_key(id: HierarchyNodeId) -> u64 {
    match id {
        HierarchyNodeId::Directory(id) => id.as_u64().wrapping_mul(2),
        HierarchyNodeId::File(id) => id.as_u64().wrapping_mul(2).wrapping_add(1),
    }
}

fn collision_direction(
    parent: DirId,
    left: HierarchyNodeId,
    right: HierarchyNodeId,
    offset: Vec2,
    distance: f32,
) -> Vec2 {
    if distance.is_finite() && distance > 0.001 {
        offset.scale(1.0 / distance)
    } else {
        hash_direction(stable_hash_numbers(&[
            0x0C01_115E,
            parent.as_u64(),
            layout_identity_key(left),
            layout_identity_key(right),
        ]))
    }
}

#[cfg(test)]
fn relax_layout_points(points: &mut [LayoutPoint], force: &SerialForceReference) {
    let mut scratch = Vec::new();
    let mut groups = Vec::new();
    relax_layout_points_grouped(points, force, None, &mut scratch, &mut groups);
}

fn relax_layout_points_grouped(
    points: &mut [LayoutPoint],
    force: &SerialForceReference,
    pool: Option<&ThreadPool>,
    scratch: &mut Vec<Vec2>,
    groups: &mut Vec<LayoutGroup>,
) {
    if points.is_empty() {
        return;
    }

    partition_layout_groups(points, groups);
    scratch.resize(points.len(), Vec2::default());
    let total_work = groups
        .iter()
        .fold(0u128, |work, group| work.saturating_add(group.pair_work));
    let repulsion = &mut scratch[..points.len()];

    if let Some(pool) =
        pool.filter(|_| groups.len() > 1 && group_split(groups, total_work).is_some())
    {
        pool.install(|| {
            relax_layout_group_range_parallel(points, repulsion, groups, 0, total_work, force);
        });
    } else {
        relax_layout_groups_serial(points, repulsion, groups, 0, force);
    }
}

fn partition_layout_groups(points: &mut [LayoutPoint], groups: &mut Vec<LayoutGroup>) {
    groups.clear();
    if points.is_empty() {
        return;
    }

    points.sort_unstable_by_key(|point| (point.parent, point.id));
    let mut start = 0;
    for end in 1..=points.len() {
        if end != points.len() && points[end].parent == points[start].parent {
            continue;
        }
        groups.push(LayoutGroup {
            start,
            end,
            pair_work: pair_work_for_len(end - start),
        });
        start = end;
    }
}

fn pair_work_for_len(len: usize) -> u128 {
    let len = len as u128;
    len.saturating_mul(len.saturating_sub(1)) / 2
}

fn group_split(groups: &[LayoutGroup], total_work: u128) -> Option<(usize, u128, u128)> {
    if groups.len() < 2 {
        return None;
    }

    let mut left_work = 0u128;
    let mut best: Option<(usize, u128, u128)> = None;
    for split in 1..groups.len() {
        left_work = left_work.saturating_add(groups[split - 1].pair_work);
        let right_work = total_work.saturating_sub(left_work);
        if left_work < MIN_PARALLEL_PAIR_WORK || right_work < MIN_PARALLEL_PAIR_WORK {
            continue;
        }
        let imbalance = left_work.abs_diff(right_work);
        if best
            .as_ref()
            .is_none_or(|(_, best_left, best_right)| imbalance < best_left.abs_diff(*best_right))
        {
            best = Some((split, left_work, right_work));
        }
    }
    best
}

fn relax_layout_groups_serial(
    points: &mut [LayoutPoint],
    scratch: &mut [Vec2],
    groups: &[LayoutGroup],
    base: usize,
    force: &SerialForceReference,
) {
    for group in groups {
        let start = group.start - base;
        let end = group.end - base;
        relax_layout_group(&mut points[start..end], force, &mut scratch[start..end]);
    }
}

fn relax_layout_group_range_parallel(
    points: &mut [LayoutPoint],
    scratch: &mut [Vec2],
    groups: &[LayoutGroup],
    base: usize,
    total_work: u128,
    force: &SerialForceReference,
) {
    let Some((split, left_work, right_work)) = group_split(groups, total_work) else {
        relax_layout_groups_serial(points, scratch, groups, base, force);
        return;
    };
    let split_at = groups[split].start - base;
    let (left_points, right_points) = points.split_at_mut(split_at);
    let (left_scratch, right_scratch) = scratch.split_at_mut(split_at);
    rayon::join(
        || {
            relax_layout_group_range_parallel(
                left_points,
                left_scratch,
                &groups[..split],
                base,
                left_work,
                force,
            );
        },
        || {
            relax_layout_group_range_parallel(
                right_points,
                right_scratch,
                &groups[split..],
                base + split_at,
                right_work,
                force,
            );
        },
    );
}

fn relax_layout_group(
    points: &mut [LayoutPoint],
    force: &SerialForceReference,
    repulsion: &mut [Vec2],
) {
    if points.is_empty() {
        return;
    }
    let repulsion = &mut repulsion[..points.len()];
    project_layout_anchors(points);
    repulsion.fill(Vec2::default());
    accumulate_repulsion_serial(points, force, repulsion);
    apply_layout_motion(points, force, repulsion);
    project_layout_positions(points);
}

fn project_layout_anchors(points: &mut [LayoutPoint]) {
    // Project colliding stable anchors apart first.  The projection is
    // recomputed from those anchors every tick rather than accumulated, so
    // siblings keep a deterministic target without endless drift.
    for _ in 0..MAX_COLLISION_PASSES {
        let mut changed = false;
        for left in 0..points.len() {
            for right in (left + 1)..points.len() {
                let offset = points[left].anchor.sub(points[right].anchor);
                let distance = offset.length();
                if !distance.is_finite() || distance >= COLLISION_DISTANCE {
                    continue;
                }
                let direction = collision_direction(
                    points[left].parent,
                    points[left].id,
                    points[right].id,
                    offset,
                    distance,
                );
                let correction = (COLLISION_DISTANCE - distance).max(0.0) * 0.5;
                points[left].anchor = points[left]
                    .anchor
                    .add(direction.scale(correction))
                    .clamped_radius(MAX_LAYOUT_RADIUS);
                points[right].anchor = points[right]
                    .anchor
                    .sub(direction.scale(correction))
                    .clamped_radius(MAX_LAYOUT_RADIUS);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

fn accumulate_repulsion_serial(
    points: &[LayoutPoint],
    force: &SerialForceReference,
    repulsion: &mut [Vec2],
) {
    for left in 0..points.len() {
        for right in (left + 1)..points.len() {
            let Some(change) = pair_repulsion_change(left, right, points, force) else {
                continue;
            };
            repulsion[left] = repulsion[left].add(change);
            repulsion[right] = repulsion[right].sub(change);
        }
    }
}

fn pair_repulsion_change(
    left: usize,
    right: usize,
    points: &[LayoutPoint],
    force: &SerialForceReference,
) -> Option<Vec2> {
    let offset = points[left].position.sub(points[right].position);
    let distance = offset.length();
    if !distance.is_finite() || distance >= COLLISION_DISTANCE {
        return None;
    }
    let overlap = (COLLISION_DISTANCE - distance) / COLLISION_DISTANCE;
    let magnitude = (force.repulsion * overlap).min(0.045);
    Some(
        collision_direction(
            points[left].parent,
            points[left].id,
            points[right].id,
            offset,
            distance,
        )
        .scale(magnitude),
    )
}

fn apply_layout_motion(
    points: &mut [LayoutPoint],
    force: &SerialForceReference,
    repulsion: &[Vec2],
) {
    for (point, change) in points.iter_mut().zip(repulsion.iter().copied()) {
        let spring = point.anchor.sub(point.position).scale(0.16);
        point.velocity = point
            .velocity
            .scale(force.damping)
            .add(spring)
            .add(change)
            .clamped_radius(MAX_LAYOUT_SPEED);
        point.position = point
            .position
            .add(point.velocity)
            .clamped_radius(MAX_LAYOUT_RADIUS);
        point.fresh = false;
    }
}

fn project_layout_positions(points: &mut [LayoutPoint]) {
    // Keep the actual positions at the same minimum, not merely their
    // eventual spring targets.  A fixed pass count makes this bounded while
    // repeated canonical ticks finish any multi-sibling projection.
    for _ in 0..MAX_COLLISION_PASSES {
        let mut changed = false;
        for left in 0..points.len() {
            for right in (left + 1)..points.len() {
                let offset = points[left].position.sub(points[right].position);
                let distance = offset.length();
                if !distance.is_finite() || distance >= COLLISION_DISTANCE {
                    continue;
                }
                let direction = collision_direction(
                    points[left].parent,
                    points[left].id,
                    points[right].id,
                    offset,
                    distance,
                );
                let correction = (COLLISION_DISTANCE - distance).max(0.0) * 0.5;
                points[left].position = points[left]
                    .position
                    .add(direction.scale(correction))
                    .clamped_radius(MAX_LAYOUT_RADIUS);
                points[right].position = points[right]
                    .position
                    .sub(direction.scale(correction))
                    .clamped_radius(MAX_LAYOUT_RADIUS);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for point in points {
        point.position = point.position.clamped_radius(MAX_LAYOUT_RADIUS);
    }
}

fn lerp(start: Vec2, end: Vec2, progress: f32) -> Vec2 {
    Vec2::new(
        start.x + (end.x - start.x) * progress,
        start.y + (end.y - start.y) * progress,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_values_reduce_exactly() {
        assert_eq!(
            RationalTime::new(120, 240).reduced().unwrap(),
            RationalTime::new(1, 2).reduced().unwrap()
        );
        assert_eq!(
            RationalTime::new(-2, -4).reduced().unwrap(),
            RationalTime::new(1, 2).reduced().unwrap()
        );
    }

    #[test]
    fn rational_minimum_values_normalize_without_panicking() {
        assert_eq!(
            RationalTime::new(0, i128::MIN).reduced(),
            Ok(RationalTime::new(0, 1))
        );
        assert_eq!(
            RationalTime::new(i128::MIN, i128::MIN).reduced(),
            Ok(RationalTime::new(1, 1))
        );
        assert_eq!(
            RationalTime::new(1, i128::MIN).reduced(),
            Err(ReplayError::RationalTimeOutOfRange)
        );
        assert_eq!(
            RationalTime::new(i128::MIN, -1).reduced(),
            Err(ReplayError::RationalTimeOutOfRange)
        );
        assert_eq!(
            RationalTime::new(1, 0).reduced(),
            Err(ReplayError::RationalTimeOutOfRange)
        );
    }

    #[test]
    fn pool_keeps_three_latest_snapshots() {
        let mut pool = SnapshotBufferPool::new();
        for tick in 0..5 {
            let mut snapshot = SceneSnapshot::empty();
            snapshot.tick = tick;
            pool.publish(Arc::new(snapshot));
        }
        assert_eq!(pool.len(), 3);
        assert_eq!(
            pool.acquire_latest().as_ref().map(|snapshot| snapshot.tick),
            Some(4)
        );
        assert!(pool.is_empty());
    }

    #[test]
    fn serial_force_is_repeatable() {
        let mut first = [Vec2::new(0.0, 0.0), Vec2::new(0.0, 0.0)];
        let mut second = first;
        let force = SerialForceReference::default();
        force.relax(&mut first);
        force.relax(&mut second);
        assert_eq!(first, second);
        assert!(first[0].x.is_finite() && first[1].x.is_finite());
    }

    #[test]
    fn path_positions_are_bounded_and_repeatable() {
        let paths = [
            "src/lib.rs",
            "src/render/shaders.wgsl",
            "README.md",
            "tests/fixtures/history.log",
            "unicode/naïve-file.txt",
        ];
        let records = [
            (0, 0, Action::Add),
            (0, 1, Action::Add),
            (0, 2, Action::Add),
            (0, 3, Action::Add),
            (0, 4, Action::Add),
        ];
        let history = history_with_actions(&paths, &records);
        let session = ReplaySession::new(history, deterministic_config()).unwrap();

        for (index, path) in paths.iter().enumerate() {
            let path_id = PathId::try_from_u64((index + 1) as u64).unwrap();
            let position = session.position_for_path(path_id);
            assert_eq!(
                position,
                session.position_for_path(path_id),
                "position changed for {path}"
            );
            assert!(
                position.x.is_finite() && position.y.is_finite(),
                "non-finite position for {path}: {position:?}"
            );
            let radius = position.x.hypot(position.y);
            assert!(radius.is_finite(), "non-finite radius for {path}");
            assert!(
                (0.7..=1.5).contains(&radius),
                "radius out of bounds for {path}: {radius}"
            );
        }
    }

    #[test]
    fn default_visual_palette_is_nonblack_and_actions_are_semantic() {
        let paths = ["src/a.rs", "src/b.py", "docs/readme.md"];
        let records = [
            (0, 0, Action::Add),
            (0, 1, Action::Add),
            (0, 2, Action::Add),
        ];
        let session = ReplaySession::new(
            history_with_actions(&paths, &records),
            deterministic_config(),
        )
        .unwrap();
        let snapshot = session.snapshot();
        assert!(snapshot.files.iter().all(|file| file.color != Rgb8::BLACK));
        assert!(
            snapshot
                .files
                .iter()
                .map(|file| file.color)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                >= 2
        );
        assert!(
            snapshot
                .directories
                .iter()
                .all(|directory| directory.color != Rgb8::BLACK)
        );
        assert!(
            snapshot
                .contributors
                .iter()
                .all(|contributor| contributor.color != Rgb8::BLACK)
        );
        assert_eq!(snapshot.actions.len(), 3);
        assert!(
            snapshot
                .actions
                .iter()
                .all(|action| action.color == ACTION_ADD_COLOR)
        );

        let mut actions = ReplaySession::new(
            history_with_actions(
                &["src/changes.rs"],
                &[
                    (0, 0, Action::Add),
                    (1, 0, Action::Modify),
                    (2, 0, Action::Delete),
                ],
            ),
            deterministic_config(),
        )
        .unwrap();
        assert_eq!(
            actions
                .snapshot()
                .actions
                .iter()
                .find(|action| action.action == Action::Add)
                .map(|action| action.color),
            Some(ACTION_ADD_COLOR)
        );
        actions.advance_ticks(120).unwrap();
        assert_eq!(
            actions
                .snapshot()
                .actions
                .iter()
                .find(|action| action.action == Action::Modify)
                .map(|action| action.color),
            Some(ACTION_MODIFY_COLOR)
        );
        actions.advance_ticks(120).unwrap();
        assert_eq!(
            actions
                .snapshot()
                .actions
                .iter()
                .find(|action| action.action == Action::Delete)
                .map(|action| action.color),
            Some(ACTION_DELETE_COLOR)
        );
    }

    #[test]
    fn action_remains_through_final_lifetime_tick_then_expires() {
        let mut session = ReplaySession::new(
            history_with_actions(&["src/action.rs"], &[(0, 0, Action::Add)]),
            deterministic_config(),
        )
        .unwrap();

        let final_tick = session.advance_ticks(ACTION_LIFETIME_TICKS).unwrap();
        assert_eq!(final_tick.tick, ACTION_LIFETIME_TICKS);
        assert_eq!(final_tick.actions.len(), 1);
        assert_eq!(final_tick.actions[0].progress, 1.0);

        let expired_tick = session.advance_ticks(1).unwrap();
        assert_eq!(expired_tick.tick, ACTION_LIFETIME_TICKS + 1);
        assert!(expired_tick.actions.is_empty());
    }

    #[test]
    fn explicit_event_color_wins_for_file_contributor_and_action() {
        let custom = Rgb8::new(1, 2, 3);
        let mut catalog = Catalog::new();
        let path = catalog.intern_path_str("custom.rs").unwrap();
        let contributor = catalog.intern_contributor("custom-user").unwrap();
        let event = Event::new(
            gource_core::EventKey::new(0, gource_core::SourceSeq::new(0).unwrap()),
            Generation::ZERO,
            contributor,
            EventTarget::File(path),
            Action::Add,
            Some(custom),
        );
        let history = gource_core::History::new(catalog, vec![event]).unwrap();
        let session = ReplaySession::new(history, deterministic_config()).unwrap();
        let snapshot = session.snapshot();
        assert_eq!(snapshot.files[0].color, custom);
        assert_eq!(snapshot.contributors[0].color, custom);
        assert_eq!(snapshot.actions[0].color, custom);
    }

    #[test]
    fn hierarchy_layout_is_bounded_and_converges_during_idle() {
        let paths = [
            "src/a.rs",
            "src/b.rs",
            "src/c.rs",
            "src/d.rs",
            "tests/a.rs",
            "tests/b.rs",
            "docs/readme.md",
        ];
        let records = [
            (0, 0, Action::Add),
            (0, 1, Action::Add),
            (0, 2, Action::Add),
            (0, 3, Action::Add),
            (0, 4, Action::Add),
            (0, 5, Action::Add),
            (0, 6, Action::Add),
        ];
        let mut session = ReplaySession::new(
            history_with_actions(&paths, &records),
            deterministic_config(),
        )
        .unwrap();
        session.advance_ticks(2_000).unwrap();
        let settled = session.snapshot();
        session.advance_ticks(2_000).unwrap();
        let later = session.snapshot();
        assert!(
            later
                .files
                .iter()
                .all(|file| file.position.x.is_finite() && file.position.y.is_finite())
        );
        assert!(later.directories.iter().all(|directory| {
            directory.position.x.is_finite() && directory.position.y.is_finite()
        }));
        for (before, after) in settled.files.iter().zip(&later.files) {
            assert!(
                before.position.sub(after.position).length() < 0.002,
                "file did not converge: {:?} -> {:?}",
                before.position,
                after.position
            );
            assert!(after.position.length() <= MAX_LAYOUT_RADIUS + 0.001);
        }
        let bounds = later.bounds().expect("active scene has bounds");
        assert!(bounds.width().is_finite() && bounds.height().is_finite());
        assert!(bounds.width() <= MAX_LAYOUT_RADIUS * 2.1);
        assert!(bounds.height() <= MAX_LAYOUT_RADIUS * 2.1);
    }

    #[test]
    fn sibling_collision_converges_to_minimum_separation() {
        let parent = DirId::new(1).expect("root directory identity");
        let mut points = (0..6)
            .map(|index| LayoutPoint {
                id: if index == 0 {
                    HierarchyNodeId::Directory(DirId::new(2).expect("directory identity"))
                } else {
                    HierarchyNodeId::File(
                        FileId::try_from_u64(index as u64).expect("file identity"),
                    )
                },
                parent,
                position: Vec2::default(),
                anchor: Vec2::default(),
                velocity: Vec2::default(),
                fresh: true,
            })
            .collect::<Vec<_>>();
        let force = SerialForceReference::default();
        for _ in 0..240 {
            relax_layout_points(&mut points, &force);
        }
        for left in 0..points.len() {
            for right in (left + 1)..points.len() {
                let distance = points[left].position.sub(points[right].position).length();
                assert!(
                    distance >= COLLISION_DISTANCE - 0.001,
                    "siblings remained colliding: {distance}"
                );
            }
        }
    }

    #[test]
    fn hierarchy_branches_and_files_use_actual_parent_positions() {
        let history = history_with_actions(
            &["src/lib/a.rs", "src/lib/b.rs", "src/main.rs"],
            &[
                (0, 0, Action::Add),
                (0, 1, Action::Add),
                (0, 2, Action::Add),
            ],
        );
        let session = ReplaySession::new(history, deterministic_config()).unwrap();
        let snapshot = session.snapshot();
        let expected_children: std::collections::BTreeSet<_> = snapshot
            .directories
            .iter()
            .map(|directory| HierarchyNodeId::Directory(directory.id))
            .chain(
                snapshot
                    .files
                    .iter()
                    .map(|file| HierarchyNodeId::File(file.file_id)),
            )
            .collect();
        let actual_children: std::collections::BTreeSet<_> = snapshot
            .branches
            .iter()
            .map(|branch| branch.child)
            .collect();
        assert_eq!(actual_children, expected_children);
        assert_eq!(snapshot.branches.len(), expected_children.len());
        for branch in &snapshot.branches {
            match branch.child {
                HierarchyNodeId::Directory(id) => {
                    if let Some(parent) = branch.parent {
                        let parent_position = snapshot
                            .directories
                            .iter()
                            .find(|directory| directory.id == parent)
                            .expect("branch parent directory")
                            .position;
                        assert_eq!(branch.start, parent_position);
                    }
                    let child_position = snapshot
                        .directories
                        .iter()
                        .find(|directory| directory.id == id)
                        .expect("branch child directory")
                        .position;
                    assert_eq!(branch.end, child_position);
                }
                HierarchyNodeId::File(id) => {
                    let file = snapshot
                        .files
                        .iter()
                        .find(|file| file.file_id == id)
                        .expect("branch file");
                    assert_eq!(branch.end, file.position);
                    if let Some(parent) = branch.parent {
                        let parent_position = snapshot
                            .directories
                            .iter()
                            .find(|directory| directory.id == parent)
                            .expect("file parent directory")
                            .position;
                        assert_eq!(branch.start, parent_position);
                        assert!(file.position.sub(parent_position).length() < 1.0);
                    }
                }
            }
        }
    }

    #[test]
    fn unrelated_sibling_does_not_move_existing_identity() {
        let base = ReplaySession::new(
            history_with_actions(&["src/a.rs"], &[(0, 0, Action::Add)]),
            deterministic_config(),
        )
        .unwrap();
        let mut base = base;
        base.advance_ticks(120).unwrap();
        let base_file = base.snapshot().files[0].position;
        let base_directory = base.snapshot().directories[0].position;

        let mut expanded = ReplaySession::new(
            history_with_actions(
                &["src/a.rs", "src/b.rs"],
                &[(0, 0, Action::Add), (1, 1, Action::Add)],
            ),
            deterministic_config(),
        )
        .unwrap();
        expanded.advance_ticks(120).unwrap();
        let expanded_file = expanded
            .snapshot()
            .files
            .iter()
            .find(|file| file.path_id == PathId::try_from_u64(1).unwrap())
            .expect("existing file")
            .position;
        let expanded_directory = expanded.snapshot().directories[0].position;
        assert_eq!(expanded_file, base_file);
        assert_eq!(expanded_directory, base_directory);
    }

    #[test]
    fn action_targets_coincide_with_live_and_fading_files() {
        let mut session = ReplaySession::new(
            history_with_actions(
                &["src/a.rs"],
                &[
                    (0, 0, Action::Add),
                    (1, 0, Action::Modify),
                    (2, 0, Action::Delete),
                ],
            ),
            deterministic_config(),
        )
        .unwrap();
        let initial = session.snapshot();
        let initial_file = initial.files.iter().find(|file| file.active).unwrap();
        let initial_action = initial
            .actions
            .iter()
            .find(|action| action.action == Action::Add)
            .unwrap();
        assert_eq!(initial_action.target, initial_file.position);

        session.advance_ticks(120).unwrap();
        let modified = session.snapshot();
        let modified_file = modified.files.iter().find(|file| file.active).unwrap();
        let modified_action = modified
            .actions
            .iter()
            .find(|action| action.action == Action::Modify)
            .unwrap();
        assert_eq!(modified_action.target, modified_file.position);

        session.advance_ticks(120).unwrap();
        let deleted = session.snapshot();
        let deleted_file = deleted.files.iter().find(|file| !file.active).unwrap();
        let delete_action = deleted
            .actions
            .iter()
            .find(|action| action.action == Action::Delete)
            .unwrap();
        assert_eq!(delete_action.target, deleted_file.position);
    }
    #[test]
    fn delete_action_keeps_old_incarnation_target_after_recreate() {
        let mut session = ReplaySession::new(
            history_with_actions(
                &["src/a.rs"],
                &[
                    (0, 0, Action::Add),
                    (1, 0, Action::Delete),
                    (2, 0, Action::Add),
                ],
            ),
            deterministic_config(),
        )
        .unwrap();

        session.advance_ticks(120).unwrap();
        let deleted = session.snapshot();
        let old_file = deleted
            .files
            .iter()
            .find(|file| !file.active)
            .expect("deleted incarnation remains during fade");
        let old_file_id = old_file.file_id;
        let old_target = old_file.position;
        let delete_state = session
            .state
            .actions
            .iter()
            .find(|action| action.action == Action::Delete)
            .expect("delete action");
        assert_eq!(delete_state.file_id, Some(old_file_id));
        assert_eq!(delete_state.target, old_target);

        session.advance_ticks(120).unwrap();
        let recreated = session.snapshot();
        let active = recreated
            .files
            .iter()
            .find(|file| file.active)
            .expect("recreated active file");
        assert_ne!(active.file_id, old_file_id);
        let delete_action = recreated
            .actions
            .iter()
            .find(|action| action.action == Action::Delete)
            .expect("delete beam survives recreation");
        assert_eq!(delete_action.target, old_target);
        assert_eq!(
            session
                .state
                .actions
                .iter()
                .find(|action| action.action == Action::Delete)
                .and_then(|action| action.file_id),
            Some(old_file_id)
        );
    }

    fn sample_history() -> gource_core::History {
        use gource_core::{
            Action, Catalog, Event, EventKey, EventTarget, Generation, History, SourceSeq,
        };

        let mut catalog = Catalog::new();
        let first = catalog.intern_path_str("src/a.rs").unwrap();
        let second = catalog.intern_path_str("src/b.rs").unwrap();
        let contributor = catalog.intern_contributor("alice").unwrap();
        let events = vec![
            Event::new(
                EventKey::new(0, SourceSeq::new(0).unwrap()),
                Generation::ZERO,
                contributor,
                EventTarget::File(first),
                Action::Add,
                Some(Rgb8::new(10, 20, 30)),
            ),
            Event::new(
                EventKey::new(1, SourceSeq::new(1).unwrap()),
                Generation::ZERO,
                contributor,
                EventTarget::File(second),
                Action::Add,
                None,
            ),
            Event::new(
                EventKey::new(2, SourceSeq::new(2).unwrap()),
                Generation::ZERO,
                contributor,
                EventTarget::File(first),
                Action::Delete,
                None,
            ),
        ];
        History::new(catalog, events).unwrap()
    }

    fn deterministic_config() -> ReplayConfig {
        ReplayConfig {
            realtime: true,
            seconds_per_day: 86_400.0,
            auto_skip_seconds: 0.0,
            ..ReplayConfig::default()
        }
    }

    #[test]
    fn wall_cadence_does_not_publish_partial_ticks() {
        let history = sample_history();
        let config = deterministic_config();
        let mut cadence = ReplaySession::new(history.clone(), config.clone()).unwrap();
        let initial = cadence.snapshot();
        cadence.advance_wall(Duration::from_millis(8)).unwrap();
        assert_eq!(cadence.snapshot().tick, initial.tick);
        cadence.advance_wall(Duration::from_millis(1)).unwrap();

        let mut fixed = ReplaySession::new(history, config).unwrap();
        fixed.advance_ticks(1).unwrap();
        assert_eq!(*cadence.snapshot(), *fixed.snapshot());
    }

    #[test]
    fn seek_and_checkpoint_restore_match_fresh_forward_replay() {
        let history = sample_history();
        let config = deterministic_config();
        let mut fresh = ReplaySession::new(history.clone(), config.clone()).unwrap();
        fresh.seek_tick(0).unwrap();
        fresh.advance_ticks(360).unwrap();
        let expected = fresh.snapshot();

        let mut sought = ReplaySession::new(history.clone(), config.clone()).unwrap();
        sought.seek_tick(360).unwrap();
        let actual = sought.snapshot();
        assert_eq!(*actual, *expected);

        let mut checkpointed = ReplaySession::new(history, config).unwrap();
        checkpointed.advance_ticks(120).unwrap();
        let checkpoint = checkpointed.checkpoint();
        let checkpoint_snapshot = checkpointed.snapshot();
        checkpointed.advance_ticks(240).unwrap();
        checkpointed.restore_checkpoint(&checkpoint).unwrap();
        assert_eq!(*checkpointed.snapshot(), *checkpoint_snapshot);
    }

    #[test]
    fn repository_seek_uses_negative_epoch_origin_and_clamps_before_it() {
        let history = history_with_actions(
            &["src/negative.rs"],
            &[(-100, 0, Action::Add), (-99, 0, Action::Modify)],
        );
        let mut session = ReplaySession::new(history, deterministic_config()).unwrap();

        let origin = session
            .seek_repository_time(RationalTime::new(-100, 1))
            .unwrap();
        assert_eq!(origin.tick, 0);
        assert_eq!(
            origin.repository_time,
            RationalTime::new(-100, 1).reduced().unwrap()
        );

        let next = session
            .seek_repository_time(RationalTime::new(-99, 1))
            .unwrap();
        assert_eq!(next.tick, 120);
        assert_eq!(
            next.repository_time,
            RationalTime::new(-99, 1).reduced().unwrap()
        );

        let before_origin = session
            .seek_repository_time(RationalTime::new(-101, 1))
            .unwrap();
        assert_eq!(before_origin.tick, 0);
        assert_eq!(
            before_origin.repository_time,
            RationalTime::new(-100, 1).reduced().unwrap()
        );
    }

    #[test]
    fn repository_seek_rejects_tick_overflow_without_replay_loop() {
        let history = history_with_actions(&["src/overflow.rs"], &[(0, 0, Action::Add)]);
        let mut session = ReplaySession::new(history, deterministic_config()).unwrap();
        let result = session.seek_repository_time(RationalTime::new(i128::from(u64::MAX) + 1, 1));
        assert_eq!(result, Err(ReplayError::RepositoryTimeOverflow));
    }

    #[test]
    fn pump_idle_gap_matches_canonical_seek_state() {
        let history = history_with_actions(
            &["src/a.rs"],
            &[(0, 0, Action::Add), (20, 0, Action::Modify)],
        );
        let mut config = deterministic_config();
        config.auto_skip_seconds = 3.0;
        let target_tick = 300;

        let mut pumped = ReplaySession::new(history.clone(), config.clone()).unwrap();
        // Match seek's generation so the complete snapshots are directly
        // comparable; the idle gate still has to integrate every tick.
        pumped.seek_tick(0).unwrap();
        while pumped.state.tick < target_tick {
            let remaining = target_tick - pumped.state.tick;
            let result = pumped.pump(WorkBudget::new(remaining.min(17), 64)).unwrap();
            assert!(result.ticks > 0, "idle pump made no canonical progress");
        }

        let mut sought = ReplaySession::new(history, config).unwrap();
        sought.seek_tick(target_tick).unwrap();
        assert_eq!(*pumped.snapshot(), *sought.snapshot());
    }

    #[test]
    fn auto_skip_is_shared_by_pump_wall_seek_and_advance_ticks() {
        const IDLE_THRESHOLD_TICKS: u64 = 120;
        let history = history_with_actions(
            &["src/a.rs"],
            &[(0, 0, Action::Add), (10, 0, Action::Modify)],
        );
        let mut enabled = deterministic_config();
        enabled.auto_skip_seconds = 1.0;

        let mut pumped = ReplaySession::new(history.clone(), enabled.clone()).unwrap();
        while pumped.state.tick < ACTION_LIFETIME_TICKS + IDLE_THRESHOLD_TICKS - 1 {
            let result = pumped.pump(WorkBudget::new(1, 64)).unwrap();
            assert_eq!(result.ticks, 1);
        }
        assert_eq!(pumped.state.idle_ticks, IDLE_THRESHOLD_TICKS - 1);
        assert_eq!(pumped.state.next_event, 1);

        // The threshold is fully reached at this tick, so the jump begins on
        // the following tick rather than shortening the configured idle span.
        pumped.pump(WorkBudget::new(1, 64)).unwrap();
        assert_eq!(
            pumped.state.tick,
            ACTION_LIFETIME_TICKS + IDLE_THRESHOLD_TICKS
        );
        assert_eq!(pumped.state.idle_ticks, IDLE_THRESHOLD_TICKS);
        assert_eq!(pumped.state.next_event, 1);
        pumped.pump(WorkBudget::new(1, 64)).unwrap();
        assert_eq!(pumped.state.next_event, 2);
        assert_eq!(pumped.repository_time(), RationalTime::new(10, 1));

        let mut advanced = ReplaySession::new(history.clone(), enabled.clone()).unwrap();
        advanced
            .advance_ticks(ACTION_LIFETIME_TICKS + IDLE_THRESHOLD_TICKS)
            .unwrap();
        assert_eq!(advanced.state.next_event, 1);
        advanced.advance_ticks(1).unwrap();
        assert_eq!(advanced.state.next_event, 2);
        assert_eq!(*advanced.snapshot(), *pumped.snapshot());

        let mut walled = ReplaySession::new(history.clone(), enabled.clone()).unwrap();
        for _ in 0..(ACTION_LIFETIME_TICKS + IDLE_THRESHOLD_TICKS) {
            walled
                .advance_wall(Duration::from_nanos(8_333_334))
                .unwrap();
        }
        walled
            .advance_wall(Duration::from_nanos(8_333_334))
            .unwrap();
        assert_eq!(walled.state.next_event, 2);
        assert_eq!(*walled.snapshot(), *pumped.snapshot());

        let mut sought = ReplaySession::new(history.clone(), enabled).unwrap();
        sought
            .seek_tick(ACTION_LIFETIME_TICKS + IDLE_THRESHOLD_TICKS)
            .unwrap();
        assert_eq!(sought.state.next_event, 1);
        sought
            .seek_tick(ACTION_LIFETIME_TICKS + IDLE_THRESHOLD_TICKS + 1)
            .unwrap();
        assert_eq!(sought.state.next_event, 2);

        let mut disabled = deterministic_config();
        disabled.auto_skip_seconds = 0.0;
        let mut no_skip = ReplaySession::new(history, disabled).unwrap();
        no_skip
            .advance_ticks(ACTION_LIFETIME_TICKS + IDLE_THRESHOLD_TICKS + 1)
            .unwrap();
        assert_eq!(no_skip.state.next_event, 1);
        let no_skip_time = no_skip.repository_time().reduced().unwrap();
        assert!(no_skip_time.numerator.div_euclid(no_skip_time.denominator) < 10);
    }

    #[test]
    fn file_idle_starts_full_fade_after_configured_tick_threshold() {
        let mut config = deterministic_config();
        config.file_idle_seconds = Some(1.0);
        let mut session = ReplaySession::new(
            history_with_actions(&["src/idle.rs"], &[(0, 0, Action::Add)]),
            config,
        )
        .unwrap();

        session.advance_ticks(120).unwrap();
        let at_threshold = session.snapshot();
        assert!(at_threshold.files[0].active);
        assert_eq!(at_threshold.files[0].opacity, 1.0);

        session.advance_ticks(1).unwrap();
        let fade_start = session.snapshot();
        assert!(!fade_start.files[0].active);
        assert_eq!(fade_start.files[0].opacity, 1.0);
        assert_eq!(
            session
                .state
                .files
                .values()
                .next()
                .expect("idle fade state")
                .fade_remaining,
            FILE_FADE_TICKS
        );

        session.advance_ticks(1).unwrap();
        let first_fade_tick = session.snapshot();
        assert_eq!(
            first_fade_tick.files[0].opacity,
            (FILE_FADE_TICKS - 1) as f32 / FILE_FADE_TICKS as f32
        );
    }

    #[test]
    fn delete_fade_has_full_start_and_exact_ninety_tick_lifetime() {
        let mut session = ReplaySession::new(
            history_with_actions(
                &["src/deleted.rs"],
                &[(0, 0, Action::Add), (1, 0, Action::Delete)],
            ),
            deterministic_config(),
        )
        .unwrap();
        session.advance_ticks(120).unwrap();
        let start = session.snapshot();
        assert_eq!(start.files.len(), 1);
        assert!(!start.files[0].active);
        assert_eq!(start.files[0].opacity, 1.0);

        session.advance_ticks(FILE_FADE_TICKS - 1).unwrap();
        let final_visible = session.snapshot();
        assert_eq!(final_visible.files.len(), 1);
        assert_eq!(final_visible.files[0].opacity, 1.0 / FILE_FADE_TICKS as f32);

        session.advance_ticks(1).unwrap();
        assert!(session.snapshot().files.is_empty());
    }

    #[test]
    fn origin_delete_uses_the_same_full_fade_start_boundary() {
        let mut session = ReplaySession::new(
            history_with_actions(
                &["src/origin-delete.rs"],
                &[(0, 0, Action::Add), (0, 0, Action::Delete)],
            ),
            deterministic_config(),
        )
        .unwrap();
        let start = session.snapshot();
        assert_eq!(start.files.len(), 1);
        assert!(!start.files[0].active);
        assert_eq!(start.files[0].opacity, 1.0);

        session.advance_ticks(1).unwrap();
        assert_eq!(
            session.snapshot().files[0].opacity,
            (FILE_FADE_TICKS - 1) as f32 / FILE_FADE_TICKS as f32
        );
    }

    fn history_with_actions(
        paths: &[&str],
        records: &[(i64, usize, Action)],
    ) -> gource_core::History {
        use gource_core::{Catalog, Event, EventKey, EventTarget, Generation, History, SourceSeq};

        let mut catalog = Catalog::new();
        let path_ids: Vec<PathId> = paths
            .iter()
            .map(|path| catalog.intern_path_str(path).unwrap())
            .collect();
        let contributor = catalog.intern_contributor("alice").unwrap();
        let events = records
            .iter()
            .enumerate()
            .map(|(source_sequence, (timestamp, path_index, action))| {
                Event::new(
                    EventKey::new(*timestamp, SourceSeq::new(source_sequence as u64).unwrap()),
                    Generation::ZERO,
                    contributor,
                    EventTarget::File(path_ids[*path_index]),
                    *action,
                    None,
                )
            })
            .collect();
        History::new(catalog, events).unwrap()
    }

    #[test]
    fn origin_timestamp_events_are_in_tick_zero_snapshot() {
        let session = ReplaySession::new(sample_history(), deterministic_config()).unwrap();
        let snapshot = session.snapshot();
        assert_eq!(snapshot.tick, 0);
        assert_eq!(snapshot.files.iter().filter(|file| file.active).count(), 1);
        assert_eq!(snapshot.directories.len(), 1);
        assert_eq!(snapshot.actions.len(), 1);
    }

    #[test]
    fn ordinary_file_events_build_compressed_nested_hierarchy() {
        let history = history_with_actions(
            &["src/lib/a.rs", "src/main.rs"],
            &[(0, 0, Action::Add), (0, 1, Action::Add)],
        );
        let session = ReplaySession::new(history, deterministic_config()).unwrap();
        let snapshot = session.snapshot();
        assert_eq!(snapshot.directories.len(), 2);
        assert!(
            snapshot
                .branches
                .iter()
                .any(|branch| branch.parent.is_some())
        );
        assert_eq!(snapshot.files.iter().filter(|file| file.active).count(), 2);
    }

    #[test]
    fn duplicate_add_replaces_world_file_incarnation() {
        let history =
            history_with_actions(&["src/a.rs"], &[(0, 0, Action::Add), (1, 0, Action::Add)]);
        let mut session = ReplaySession::new(history, deterministic_config()).unwrap();
        let initial = session.snapshot();
        let initial_incarnation = initial
            .files
            .iter()
            .find(|file| file.active)
            .map(|file| file.incarnation)
            .unwrap();
        session.advance_ticks(120).unwrap();
        let snapshot = session.snapshot();
        let active: Vec<_> = snapshot.files.iter().filter(|file| file.active).collect();
        assert_eq!(active.len(), 1);
        assert!(active[0].incarnation > initial_incarnation);
        assert!(
            snapshot
                .files
                .iter()
                .any(|file| !file.active && file.incarnation == initial_incarnation)
        );
    }

    #[test]
    fn missing_modify_is_reported_by_core_world() {
        let history = history_with_actions(&["missing.rs"], &[(0, 0, Action::Modify)]);
        let result = ReplaySession::new(history, deterministic_config());
        assert!(matches!(
            result,
            Err(ReplayError::World(WorldError::MissingFile(_)))
        ));
    }

    #[test]
    fn equal_time_burst_never_publishes_partial_tick() {
        let mut paths = vec!["origin"];
        paths.extend(["a", "b", "c", "d", "e"]);
        let records = [
            (0, 0, Action::Add),
            (1, 1, Action::Add),
            (1, 2, Action::Add),
            (1, 3, Action::Add),
            (1, 4, Action::Add),
            (1, 5, Action::Add),
        ];
        let history = history_with_actions(&paths, &records);
        let config = deterministic_config();
        let mut session = ReplaySession::new(history.clone(), config.clone()).unwrap();
        session.seek_tick(119).unwrap();

        let first = session.pump(WorkBudget::new(1, 2)).unwrap();
        assert_eq!(
            (first.ticks, first.events, first.snapshot_tick),
            (0, 2, 119)
        );
        let second = session.pump(WorkBudget::new(1, 2)).unwrap();
        assert_eq!(
            (second.ticks, second.events, second.snapshot_tick),
            (0, 2, 119)
        );
        let third = session.pump(WorkBudget::new(1, 2)).unwrap();
        assert_eq!(
            (third.ticks, third.events, third.snapshot_tick),
            (1, 1, 120)
        );

        let mut expected = ReplaySession::new(history, config).unwrap();
        expected.seek_tick(120).unwrap();
        assert_eq!(*session.snapshot(), *expected.snapshot());
    }

    #[test]
    fn checkpoints_reject_same_ids_with_different_catalog_content() {
        let left_history = history_with_actions(&["left/a.rs"], &[(0, 0, Action::Add)]);
        let right_history = history_with_actions(&["right/a.rs"], &[(0, 0, Action::Add)]);
        let config = deterministic_config();
        let mut left = ReplaySession::new(left_history, config.clone()).unwrap();
        let checkpoint = left.checkpoint();
        let mut right = ReplaySession::new(right_history, config).unwrap();
        assert_eq!(
            right.restore_checkpoint(&checkpoint),
            Err(ReplayError::CheckpointIdentityMismatch)
        );
    }
}
