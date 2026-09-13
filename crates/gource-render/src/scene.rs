// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! CPU-side geometry preparation shared by native and headless rendering.

use std::cmp::Ordering;
use std::f32::consts::TAU;

/// A two-dimensional render coordinate.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C)]
pub struct RenderPoint {
    pub x: f32,
    pub y: f32,
}

impl RenderPoint {
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    #[must_use]
    pub fn finite(self) -> Self {
        Self {
            x: if self.x.is_finite() { self.x } else { 0.0 },
            y: if self.y.is_finite() { self.y } else { 0.0 },
        }
    }
}

/// A premultiplied RGBA colour in the shader's normalized range.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct PremultipliedColor(pub [f32; 4]);

impl PremultipliedColor {
    #[must_use]
    pub const fn new(value: [f32; 4]) -> Self {
        Self(value)
    }

    /// Build a premultiplied colour from an 8-bit RGB triplet and opacity.
    #[must_use]
    pub fn from_rgb8(rgb: [u8; 3], opacity: f32) -> Self {
        let alpha = opacity.clamp(0.0, 1.0);
        let scale = alpha / 255.0;
        Self([
            f32::from(rgb[0]) * scale,
            f32::from(rgb[1]) * scale,
            f32::from(rgb[2]) * scale,
            alpha,
        ])
    }
}

impl Default for PremultipliedColor {
    fn default() -> Self {
        Self([1.0, 1.0, 1.0, 1.0])
    }
}

/// A vertex used by the branch and action triangle batches.
#[derive(bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TriangleVertex {
    pub position: [f32; 2],
    pub color: [f32; 4],
}

/// Per-instance geometry for a file or contributor node.
///
/// The final bytes are explicit padding.  Keeping this at 48 bytes makes the
/// WGSL vertex layout unambiguous across backends.
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct NodeInstance {
    pub center: [f32; 2],
    pub radius: f32,
    pub opacity: f32,
    pub color: [f32; 4],
    pub kind: u32,
    pub _padding: [u32; 3],
}

/// A unit-square vertex used for instanced node geometry.
#[derive(bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct QuadVertex {
    pub corner: [f32; 2],
}

/// A triangle vertex used by the batched bitmap glyph geometry.
///
/// Labels are deliberately prepared as clip-space triangles so the same
/// geometry can be submitted to both the supplied native target and an
/// offscreen export target without another texture-generation pass.
#[derive(bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LabelVertex {
    pub position: [f32; 2],
    pub color: [f32; 4],
}

/// The world-space advance of one label glyph before camera projection.
///
/// Keeping these dimensions in world units makes text track the same camera
/// zoom as nodes while the final projection still accounts for aspect ratio.
pub const LABEL_GLYPH_ADVANCE_WORLD: f32 = 0.018;
/// The world-space height of the seven-row embedded glyph atlas.
pub const LABEL_GLYPH_HEIGHT_WORLD: f32 = 0.024;
const LABEL_GLYPH_COLUMNS: usize = 5;
const LABEL_GLYPH_ROWS: usize = 7;
const LABEL_GLYPH_MAX_CELLS: usize = LABEL_GLYPH_COLUMNS * LABEL_GLYPH_ROWS;
/// Candidate layout is bounded independently of the caller's string length.
pub(crate) const MAX_LABEL_LAYOUT_GLYPHS: usize = 256;

const fn glyph(rows: [u8; LABEL_GLYPH_ROWS]) -> u64 {
    let mut packed = 0_u64;
    let mut row = 0;
    while row < LABEL_GLYPH_ROWS {
        packed |= (rows[row] as u64 & 0x1f) << (row * LABEL_GLYPH_COLUMNS);
        row += 1;
    }
    packed
}

// This is a renderer-owned 5x7 vector/bitmap atlas.  It contains no external
// asset and is intentionally small enough to remain deterministic and bounded.
// Lowercase glyphs have distinct forms so ASCII paths retain their spelling.
const UPPERCASE_GLYPHS: [u64; 26] = [
    glyph([14, 17, 17, 31, 17, 17, 17]),
    glyph([30, 17, 17, 30, 17, 17, 30]),
    glyph([14, 17, 16, 16, 16, 17, 14]),
    glyph([30, 17, 17, 17, 17, 17, 30]),
    glyph([31, 16, 16, 30, 16, 16, 31]),
    glyph([31, 16, 16, 30, 16, 16, 16]),
    glyph([14, 17, 16, 23, 17, 17, 15]),
    glyph([17, 17, 17, 31, 17, 17, 17]),
    glyph([14, 4, 4, 4, 4, 4, 14]),
    glyph([7, 2, 2, 2, 2, 18, 12]),
    glyph([17, 18, 20, 24, 20, 18, 17]),
    glyph([16, 16, 16, 16, 16, 16, 31]),
    glyph([17, 27, 21, 21, 17, 17, 17]),
    glyph([17, 25, 21, 19, 17, 17, 17]),
    glyph([14, 17, 17, 17, 17, 17, 14]),
    glyph([30, 17, 17, 30, 16, 16, 16]),
    glyph([14, 17, 17, 17, 21, 18, 13]),
    glyph([30, 17, 17, 30, 20, 18, 17]),
    glyph([15, 16, 16, 14, 1, 1, 30]),
    glyph([31, 4, 4, 4, 4, 4, 4]),
    glyph([17, 17, 17, 17, 17, 17, 14]),
    glyph([17, 17, 17, 17, 17, 10, 4]),
    glyph([17, 17, 17, 21, 21, 21, 10]),
    glyph([17, 17, 10, 4, 10, 17, 17]),
    glyph([17, 17, 10, 4, 4, 4, 4]),
    glyph([31, 1, 2, 4, 8, 16, 31]),
];

const LOWERCASE_GLYPHS: [u64; 26] = [
    glyph([0, 0, 14, 1, 15, 17, 15]),
    glyph([16, 16, 30, 17, 17, 17, 30]),
    glyph([0, 0, 14, 16, 16, 17, 14]),
    glyph([1, 1, 15, 17, 17, 17, 15]),
    glyph([0, 0, 14, 17, 31, 16, 14]),
    glyph([6, 9, 8, 28, 8, 8, 8]),
    glyph([0, 0, 15, 17, 15, 1, 14]),
    glyph([16, 16, 30, 17, 17, 17, 17]),
    glyph([4, 0, 12, 4, 4, 4, 14]),
    glyph([2, 0, 6, 2, 2, 18, 12]),
    glyph([16, 16, 18, 20, 24, 20, 18]),
    glyph([12, 4, 4, 4, 4, 4, 14]),
    glyph([0, 0, 26, 21, 21, 17, 17]),
    glyph([0, 0, 30, 17, 17, 17, 17]),
    glyph([0, 0, 14, 17, 17, 17, 14]),
    glyph([0, 0, 30, 17, 30, 16, 16]),
    glyph([0, 0, 15, 17, 15, 1, 1]),
    glyph([0, 0, 22, 25, 16, 16, 16]),
    glyph([0, 0, 15, 16, 14, 1, 30]),
    glyph([8, 8, 28, 8, 8, 9, 6]),
    glyph([0, 0, 17, 17, 17, 19, 13]),
    glyph([0, 0, 17, 17, 17, 10, 4]),
    glyph([0, 0, 17, 17, 21, 21, 10]),
    glyph([0, 0, 17, 10, 4, 10, 17]),
    glyph([0, 0, 17, 17, 15, 1, 14]),
    glyph([0, 0, 31, 2, 4, 8, 31]),
];

const DIGIT_GLYPHS: [u64; 10] = [
    glyph([14, 17, 19, 21, 25, 17, 14]),
    glyph([4, 12, 4, 4, 4, 4, 14]),
    glyph([14, 17, 1, 2, 4, 8, 31]),
    glyph([30, 1, 1, 14, 1, 1, 30]),
    glyph([2, 6, 10, 18, 31, 2, 2]),
    glyph([31, 16, 16, 30, 1, 1, 30]),
    glyph([6, 8, 16, 30, 17, 17, 14]),
    glyph([31, 1, 2, 4, 8, 8, 8]),
    glyph([14, 17, 17, 14, 17, 17, 14]),
    glyph([14, 17, 17, 15, 1, 2, 12]),
];

const FALLBACK_GLYPH: u64 = glyph([31, 17, 21, 21, 21, 17, 31]);

fn glyph_bits(character: char) -> u64 {
    if character.is_ascii_uppercase() {
        return UPPERCASE_GLYPHS[(character as usize) - ('A' as usize)];
    }
    if character.is_ascii_lowercase() {
        return LOWERCASE_GLYPHS[(character as usize) - ('a' as usize)];
    }
    if character.is_ascii_digit() {
        return DIGIT_GLYPHS[(character as usize) - ('0' as usize)];
    }
    match character {
        ' ' => glyph([0, 0, 0, 0, 0, 0, 0]),
        '!' => glyph([4, 4, 4, 4, 4, 0, 4]),
        '"' => glyph([10, 10, 0, 0, 0, 0, 0]),
        '#' => glyph([10, 31, 10, 10, 31, 10, 10]),
        '$' => glyph([4, 15, 20, 14, 5, 30, 4]),
        '%' => glyph([25, 26, 4, 8, 19, 11, 0]),
        '&' => glyph([12, 18, 20, 8, 21, 18, 13]),
        '\'' => glyph([4, 4, 0, 0, 0, 0, 0]),
        '(' => glyph([2, 4, 8, 8, 8, 4, 2]),
        ')' => glyph([8, 4, 2, 2, 2, 4, 8]),
        '*' => glyph([0, 4, 21, 14, 21, 4, 0]),
        '+' => glyph([0, 4, 4, 31, 4, 4, 0]),
        ',' => glyph([0, 0, 0, 0, 0, 4, 8]),
        '-' => glyph([0, 0, 0, 31, 0, 0, 0]),
        '.' => glyph([0, 0, 0, 0, 0, 0, 4]),
        '/' => glyph([1, 2, 4, 8, 16, 0, 0]),
        ':' => glyph([0, 0, 4, 0, 0, 4, 0]),
        ';' => glyph([0, 0, 4, 0, 0, 4, 8]),
        '<' => glyph([2, 4, 8, 16, 8, 4, 2]),
        '=' => glyph([0, 0, 31, 0, 31, 0, 0]),
        '>' => glyph([8, 4, 2, 1, 2, 4, 8]),
        '?' => glyph([14, 17, 1, 2, 4, 0, 4]),
        '@' => glyph([14, 17, 23, 21, 23, 16, 14]),
        '[' => glyph([14, 8, 8, 8, 8, 8, 14]),
        '\\' => glyph([16, 8, 4, 2, 1, 0, 0]),
        ']' => glyph([14, 2, 2, 2, 2, 2, 14]),
        '^' => glyph([4, 10, 17, 0, 0, 0, 0]),
        '_' => glyph([0, 0, 0, 0, 0, 0, 31]),
        '`' => glyph([8, 4, 0, 0, 0, 0, 0]),
        '{' => glyph([2, 4, 4, 8, 4, 4, 2]),
        '|' => glyph([4, 4, 4, 4, 4, 4, 4]),
        '}' => glyph([8, 4, 4, 2, 4, 4, 8]),
        '~' => glyph([0, 0, 9, 18, 0, 0, 0]),
        // Unicode and non-printable input use a hollow-box marker.  It is
        // deliberately visible, deterministic, and never silently dropped.
        _ => FALLBACK_GLYPH,
    }
}

fn glyph_cell_count(bits: u64) -> usize {
    (bits & ((1_u64 << (LABEL_GLYPH_MAX_CELLS)) - 1)).count_ones() as usize
}

/// A render-only label candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct LabelCandidate {
    pub stable_key: u64,
    pub position: RenderPoint,
    pub width: f32,
    pub priority: u32,
    pub visibility: f32,
    pub opacity: f32,
    pub color: [u8; 3],
    pub text: String,
}

/// The camera and target extent used for culling and world-to-NDC mapping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderView {
    pub width: u32,
    pub height: u32,
    pub center: RenderPoint,
    pub zoom: f32,
    pub rotation_radians: f32,
}

impl Default for RenderView {
    fn default() -> Self {
        Self {
            width: 1,
            height: 1,
            center: RenderPoint::new(0.0, 0.0),
            zoom: 1.0,
            rotation_radians: 0.0,
        }
    }
}

impl RenderView {
    /// Validate an extent before it is used to create a viewport uniform.
    pub fn validate(&self) -> Result<(), RenderExtentError> {
        if self.width == 0 || self.height == 0 {
            return Err(RenderExtentError {
                width: self.width,
                height: self.height,
            });
        }
        if !self.zoom.is_finite() || self.zoom <= 0.0 {
            return Err(RenderExtentError {
                width: self.width,
                height: self.height,
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn aspect(self) -> f32 {
        self.width as f32 / self.height.max(1) as f32
    }
}

/// An actionable error for an invalid target extent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderExtentError {
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Display for RenderExtentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "render target extent {}x{} is invalid; width and height must both be non-zero and zoom must be finite",
            self.width, self.height
        )
    }
}

impl std::error::Error for RenderExtentError {}

/// Hard limits for CPU and GPU geometry allocations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceLimits {
    pub max_buffer_bytes: u64,
    pub max_triangle_vertices: usize,
    pub max_node_instances: usize,
    pub max_label_vertices: usize,
    /// Maximum Unicode scalar values converted to glyph geometry per scene.
    pub max_label_glyphs: usize,
    pub max_labels: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_buffer_bytes: 64 * 1024 * 1024,
            max_triangle_vertices: 1_000_000,
            max_node_instances: 500_000,
            max_label_vertices: 1_000_000,
            max_label_glyphs: 16_384,
            max_labels: 100_000,
        }
    }
}

/// Renderer-level configuration.  The target format is intentionally fixed to
/// `Rgba8Unorm` by the scene contract.
#[derive(Clone, Debug, PartialEq)]
pub struct RendererConfig {
    pub view: RenderView,
    pub resource_limits: ResourceLimits,
    pub label_budget: usize,
    pub clear_color: [f64; 4],
}

impl Default for RendererConfig {
    fn default() -> Self {
        Self {
            view: RenderView::default(),
            resource_limits: ResourceLimits::default(),
            label_budget: 4096,
            clear_color: [0.015, 0.02, 0.03, 1.0],
        }
    }
}

/// CPU geometry prepared for one scene snapshot.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PreparedScene {
    pub branch_vertices: Vec<TriangleVertex>,
    pub action_vertices: Vec<TriangleVertex>,
    pub node_instances: Vec<NodeInstance>,
    pub contributor_instances: Vec<NodeInstance>,
    pub label_vertices: Vec<LabelVertex>,
    pub label_glyphs: usize,
    pub visible_labels: usize,
    pub culled_objects: usize,
}

impl PreparedScene {
    /// Reset lengths and counters while retaining all geometry allocations.
    pub(crate) fn clear(&mut self) {
        self.branch_vertices.clear();
        self.action_vertices.clear();
        self.node_instances.clear();
        self.contributor_instances.clear();
        self.label_vertices.clear();
        self.label_glyphs = 0;
        self.visible_labels = 0;
        self.culled_objects = 0;
    }
}

/// Return a geometrically grown capacity without exceeding `maximum`.
///
/// Growth doubles the old allocation (or starts at one) and then clamps to the
/// requested size.  The function is intentionally pure so callers can test
/// cap behaviour without allocating GPU resources.
pub fn grown_capacity(
    current: usize,
    requested: usize,
    maximum: usize,
) -> Result<usize, ResourceLimitError> {
    if requested > maximum {
        return Err(ResourceLimitError {
            resource: "geometry items",
            requested,
            maximum,
        });
    }
    if requested <= current {
        return Ok(current);
    }
    let mut grown = current.max(1);
    while grown < requested {
        let next = grown.saturating_mul(2);
        if next <= grown {
            grown = requested;
            break;
        }
        grown = next.min(maximum);
    }
    Ok(grown.max(requested))
}

/// Validate both item and byte limits for one allocation.
pub fn checked_buffer_bytes(
    resource: &'static str,
    item_count: usize,
    item_size: usize,
    max_items: usize,
    max_bytes: u64,
) -> Result<u64, ResourceLimitError> {
    if item_count > max_items {
        return Err(ResourceLimitError {
            resource,
            requested: item_count,
            maximum: max_items,
        });
    }
    let bytes = item_count
        .checked_mul(item_size)
        .ok_or(ResourceLimitError {
            resource,
            requested: usize::MAX,
            maximum: max_bytes.min(usize::MAX as u64) as usize,
        })?;
    if bytes as u64 > max_bytes {
        return Err(ResourceLimitError {
            resource,
            requested: bytes,
            maximum: max_bytes.min(usize::MAX as u64) as usize,
        });
    }
    Ok(bytes as u64)
}

/// A bounded allocation failure; no geometry is silently dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceLimitError {
    pub resource: &'static str,
    pub requested: usize,
    pub maximum: usize,
}

impl std::fmt::Display for ResourceLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "resource limit exceeded for {}: requested {}, maximum {}",
            self.resource, self.requested, self.maximum
        )
    }
}

impl std::error::Error for ResourceLimitError {}

/// Convert a world coordinate to normalized device coordinates.
#[must_use]
pub fn world_to_ndc(point: RenderPoint, view: RenderView) -> [f32; 2] {
    let point = point.finite();
    let center = view.center.finite();
    let dx = point.x - center.x;
    let dy = point.y - center.y;
    let angle = if view.rotation_radians.is_finite() {
        view.rotation_radians.rem_euclid(TAU)
    } else {
        0.0
    };
    let (sin, cos) = angle.sin_cos();
    let rotated_x = (dx * cos - dy * sin) * view.zoom;
    let rotated_y = (dx * sin + dy * cos) * view.zoom;
    let aspect = view.aspect();
    [rotated_x / aspect, rotated_y]
}

/// Test a circular object against the camera's conservative view rectangle.
#[must_use]
pub fn circle_visible(point: RenderPoint, radius: f32, view: RenderView) -> bool {
    if view.validate().is_err() {
        return false;
    }
    let ndc = world_to_ndc(point, view);
    let safe_radius = if radius.is_finite() {
        radius.abs() * view.zoom
    } else {
        0.0
    };
    // World-space circles become ellipses in NDC when the target is not
    // square.  Keep each axis conservative so portrait targets do not cull
    // circles that intersect the horizontal edge.
    let aspect = view.aspect().max(f32::MIN_POSITIVE);
    let radius_x = safe_radius / aspect;
    let radius_y = safe_radius;
    ndc[0] + radius_x >= -1.0
        && ndc[0] - radius_x <= 1.0
        && ndc[1] + radius_y >= -1.0
        && ndc[1] - radius_y <= 1.0
}

/// Select at most `budget` labels deterministically.
///
/// This public convenience wrapper owns its temporary selection storage.  The
/// renderer uses [`select_label_indices_into`] so that the same storage can be
/// retained across frames.
///
/// Selection is independent of input order: higher priority, then visibility
/// score, then stable key, then text are preferred.  Off-screen and
/// overlapping candidates are omitted before the budget is consumed.  Since
/// labels are considered in priority order, an active label wins an overlap.
pub fn select_label_indices(
    candidates: &[LabelCandidate],
    budget: usize,
    view: RenderView,
) -> Vec<usize> {
    let mut scratch = LabelSelectionScratch::default();
    let selected = select_label_indices_into(candidates, budget, view, &mut scratch);
    scratch.selected[..selected].to_vec()
}

/// Reusable storage for deterministic label selection.
///
/// The vectors are deliberately kept separate: candidate ordering is sorted
/// in place, while selected indices and their bounds are consumed together by
/// the geometry preparation pass.
#[derive(Default)]
pub(crate) struct LabelSelectionScratch {
    indices: Vec<usize>,
    selected: Vec<usize>,
    selected_bounds: Vec<LabelBounds>,
}

impl LabelSelectionScratch {
    pub(crate) fn selected(&self) -> &[usize] {
        &self.selected
    }
}

/// Fill reusable selection storage and return the number of selected labels.
pub(crate) fn select_label_indices_into(
    candidates: &[LabelCandidate],
    budget: usize,
    view: RenderView,
    scratch: &mut LabelSelectionScratch,
) -> usize {
    scratch.indices.clear();
    scratch.selected.clear();
    scratch.selected_bounds.clear();
    if budget == 0 || view.validate().is_err() {
        return 0;
    }
    if scratch.indices.capacity() < candidates.len() {
        scratch
            .indices
            .reserve_exact(candidates.len() - scratch.indices.capacity());
    }
    scratch.indices.extend(
        candidates
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| {
                (finite_score(candidate.visibility * candidate.opacity) > 0.0
                    && circle_visible(candidate.position, candidate_world_width(candidate), view))
                .then_some(index)
            }),
    );
    scratch.indices.sort_unstable_by(|left_index, right_index| {
        let left = &candidates[*left_index];
        let right = &candidates[*right_index];
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| {
                let left_score = finite_score(left.visibility * left.opacity);
                let right_score = finite_score(right.visibility * right.opacity);
                right_score
                    .partial_cmp(&left_score)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| left.stable_key.cmp(&right.stable_key))
            .then_with(|| left.text.cmp(&right.text))
            .then_with(|| left_index.cmp(right_index))
    });

    let selected_capacity = budget.min(scratch.indices.len());
    if scratch.selected.capacity() < selected_capacity {
        scratch
            .selected
            .reserve_exact(selected_capacity - scratch.selected.capacity());
    }
    if scratch.selected_bounds.capacity() < selected_capacity {
        scratch
            .selected_bounds
            .reserve_exact(selected_capacity - scratch.selected_bounds.capacity());
    }
    for &index in &scratch.indices {
        if scratch.selected.len() >= budget {
            break;
        }
        let bounds = label_bounds(&candidates[index], view);
        if scratch
            .selected_bounds
            .iter()
            .any(|other| bounds_overlap(bounds, *other))
        {
            continue;
        }
        scratch.selected.push(index);
        scratch.selected_bounds.push(bounds);
    }
    scratch.selected.len()
}

#[derive(Clone, Copy)]
struct LabelBounds {
    left: f32,
    bottom: f32,
    right: f32,
    top: f32,
}

fn candidate_world_width(candidate: &LabelCandidate) -> f32 {
    if candidate.width.is_finite() {
        candidate.width.abs().clamp(0.002, 0.9)
    } else {
        0.002
    }
}

fn label_bounds(candidate: &LabelCandidate, view: RenderView) -> LabelBounds {
    let center = world_to_ndc(candidate.position, view);
    let width = candidate_world_width(candidate) * view.zoom / view.aspect().max(f32::MIN_POSITIVE);
    let prominence = 1.0 + candidate.priority.min(4) as f32 * 0.12;
    let height = LABEL_GLYPH_HEIGHT_WORLD * view.zoom * prominence;
    LabelBounds {
        left: center[0] - width * 0.5,
        right: center[0] + width * 0.5,
        bottom: center[1] - height * 0.15,
        top: center[1] + height * 1.05,
    }
}

fn bounds_overlap(left: LabelBounds, right: LabelBounds) -> bool {
    const OVERLAP_PADDING: f32 = 0.004;
    left.left < right.right + OVERLAP_PADDING
        && left.right + OVERLAP_PADDING > right.left
        && left.bottom < right.top + OVERLAP_PADDING
        && left.top + OVERLAP_PADDING > right.bottom
}

fn finite_score(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Append a thick segment represented by two triangles.
pub fn append_segment(
    output: &mut Vec<TriangleVertex>,
    start: RenderPoint,
    end: RenderPoint,
    width: f32,
    color: [f32; 4],
) {
    let start = start.finite();
    let end = end.finite();
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let length = (dx * dx + dy * dy).sqrt();
    if !length.is_finite() || length <= f32::EPSILON {
        return;
    }
    let half = width.abs().max(0.0001) * 0.5;
    let px = -dy / length * half;
    let py = dx / length * half;
    let a = [start.x + px, start.y + py];
    let b = [start.x - px, start.y - py];
    let c = [end.x - px, end.y - py];
    let d = [end.x + px, end.y + py];
    output.extend([
        TriangleVertex { position: a, color },
        TriangleVertex { position: b, color },
        TriangleVertex { position: c, color },
        TriangleVertex { position: a, color },
        TriangleVertex { position: c, color },
        TriangleVertex { position: d, color },
    ]);
}

/// Append a small directional triangle used for activity/action markers.
pub fn append_action_triangle(
    output: &mut Vec<TriangleVertex>,
    center: RenderPoint,
    direction: RenderPoint,
    radius: f32,
    color: [f32; 4],
) {
    let direction = direction.finite();
    let length = (direction.x * direction.x + direction.y * direction.y).sqrt();
    let (ux, uy) = if length > f32::EPSILON && length.is_finite() {
        (direction.x / length, direction.y / length)
    } else {
        (1.0, 0.0)
    };
    let side = radius.abs().max(0.0001);
    let tip = RenderPoint::new(center.x + ux * side * 2.0, center.y + uy * side * 2.0);
    let left = RenderPoint::new(
        center.x - ux * side + uy * side,
        center.y - uy * side - ux * side,
    );
    let right = RenderPoint::new(
        center.x - ux * side - uy * side,
        center.y - uy * side + ux * side,
    );
    output.extend([
        TriangleVertex {
            position: [tip.x, tip.y],
            color,
        },
        TriangleVertex {
            position: [left.x, left.y],
            color,
        },
        TriangleVertex {
            position: [right.x, right.y],
            color,
        },
    ]);
}

/// Count glyphs that can be laid out from one candidate under a caller budget.
///
/// Unicode scalar values are intentionally counted one-for-one: unsupported
/// values receive the visible hollow-box fallback from [`glyph_bits`].
pub(crate) fn label_glyph_count(text: &str, budget: usize) -> usize {
    text.chars()
        .take(budget.min(MAX_LABEL_LAYOUT_GLYPHS))
        .count()
}

/// Count the triangle vertices needed by at most `budget` glyphs.
pub(crate) fn label_vertex_count(text: &str, budget: usize) -> usize {
    text.chars()
        .take(budget.min(MAX_LABEL_LAYOUT_GLYPHS))
        .map(|character| glyph_cell_count(glyph_bits(character)).saturating_mul(6))
        .sum()
}

fn append_label_quad(
    output: &mut Vec<LabelVertex>,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    color: [f32; 4],
) {
    output.extend([
        LabelVertex {
            position: [x0, y0],
            color,
        },
        LabelVertex {
            position: [x0, y1],
            color,
        },
        LabelVertex {
            position: [x1, y1],
            color,
        },
        LabelVertex {
            position: [x0, y0],
            color,
        },
        LabelVertex {
            position: [x1, y1],
            color,
        },
        LabelVertex {
            position: [x1, y0],
            color,
        },
    ]);
}

/// Append readable, batched glyph geometry for one label.
///
/// The atlas is a deterministic 5x7 bitmap represented as tiny clip-space
/// quads.  This avoids a second texture-generation/render pass while keeping
/// all labels in one vertex buffer and draw call.  Every unsupported Unicode
/// scalar uses a visible hollow-box fallback, so text is never silently
/// omitted.  The returned value is the number of glyphs actually emitted.
pub fn append_label_glyphs(
    output: &mut Vec<LabelVertex>,
    candidate: &LabelCandidate,
    view: RenderView,
    glyph_budget: usize,
) -> usize {
    if glyph_budget == 0 || view.validate().is_err() {
        return 0;
    }
    let glyph_budget = glyph_budget.min(MAX_LABEL_LAYOUT_GLYPHS);
    let glyph_count = label_glyph_count(&candidate.text, glyph_budget);
    if glyph_count == 0 {
        return 0;
    }

    let center = world_to_ndc(candidate.position, view);
    let aspect = view.aspect().max(f32::MIN_POSITIVE);
    let width = (candidate_world_width(candidate) * view.zoom / aspect).clamp(0.002, 1.8);
    let prominence = 1.0 + candidate.priority.min(4) as f32 * 0.12;
    let height = LABEL_GLYPH_HEIGHT_WORLD * view.zoom * prominence;
    let advance = width / glyph_count as f32;
    let cell_width = advance / (LABEL_GLYPH_COLUMNS as f32 + 1.0);
    let cell_height = height / LABEL_GLYPH_ROWS as f32;
    let x_origin = center[0] - width * 0.5;
    let y_top = center[1] + height * 1.05;
    let opacity = if candidate.opacity.is_finite() {
        (candidate.opacity.clamp(0.0, 1.0) * (0.82 + prominence * 0.18)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    if opacity <= 0.0 {
        return 0;
    }
    let color = PremultipliedColor::from_rgb8(candidate.color, opacity).0;
    for (glyph_index, character) in candidate.text.chars().take(glyph_count).enumerate() {
        let bits = glyph_bits(character);
        let glyph_origin = x_origin + glyph_index as f32 * advance;
        for row in 0..LABEL_GLYPH_ROWS {
            let row_bits = ((bits >> (row * LABEL_GLYPH_COLUMNS)) & 0x1f) as u8;
            for column in 0..LABEL_GLYPH_COLUMNS {
                if row_bits & (1 << (LABEL_GLYPH_COLUMNS - 1 - column)) == 0 {
                    continue;
                }
                let x0 = glyph_origin + column as f32 * cell_width;
                let y1 = y_top - row as f32 * cell_height;
                let y0 = y1 - cell_height * 0.88;
                append_label_quad(output, x0, y0, x0 + cell_width * 0.88, y1, color);
            }
        }
    }
    glyph_count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growth_is_geometric_and_capped() {
        assert_eq!(grown_capacity(0, 1, 16).unwrap(), 1);
        assert_eq!(grown_capacity(1, 2, 16).unwrap(), 2);
        assert_eq!(grown_capacity(2, 3, 16).unwrap(), 4);
        assert_eq!(grown_capacity(8, 9, 16).unwrap(), 16);
        assert!(grown_capacity(16, 17, 16).is_err());
    }

    #[test]
    fn prepared_clear_retains_geometry_capacity() {
        let mut prepared = PreparedScene::default();
        prepared
            .branch_vertices
            .extend([TriangleVertex::default(); 6]);
        prepared.node_instances.extend([NodeInstance::default(); 2]);
        prepared.label_vertices.extend([LabelVertex::default(); 6]);
        let capacities = (
            prepared.branch_vertices.capacity(),
            prepared.node_instances.capacity(),
            prepared.label_vertices.capacity(),
        );
        prepared.label_glyphs = 1;
        prepared.visible_labels = 1;
        prepared.culled_objects = 1;
        prepared.clear();
        assert_eq!(prepared.branch_vertices.len(), 0);
        assert_eq!(prepared.node_instances.len(), 0);
        assert_eq!(prepared.label_vertices.len(), 0);
        assert_eq!(prepared.label_glyphs, 0);
        assert_eq!(prepared.visible_labels, 0);
        assert_eq!(prepared.culled_objects, 0);
        assert_eq!(
            capacities,
            (
                prepared.branch_vertices.capacity(),
                prepared.node_instances.capacity(),
                prepared.label_vertices.capacity()
            )
        );
    }

    #[test]
    fn bytes_are_checked_without_overflow() {
        assert_eq!(checked_buffer_bytes("x", 3, 4, 4, 16).unwrap(), 12);
        assert!(checked_buffer_bytes("x", 5, 4, 5, 16).is_err());
        assert!(checked_buffer_bytes("x", usize::MAX, 2, usize::MAX, u64::MAX).is_err());
    }

    #[test]
    fn labels_are_order_independent_and_budgeted() {
        let view = RenderView {
            width: 100,
            height: 100,
            ..RenderView::default()
        };
        let mut candidates = vec![
            LabelCandidate {
                stable_key: 4,
                position: RenderPoint::new(0.0, 0.0),
                width: 0.1,
                priority: 1,
                visibility: 1.0,
                opacity: 1.0,
                color: [255, 255, 255],
                text: "late".to_owned(),
            },
            LabelCandidate {
                stable_key: 2,
                position: RenderPoint::new(0.0, 0.0),
                width: 0.1,
                priority: 2,
                visibility: 1.0,
                opacity: 1.0,
                color: [255, 255, 255],
                text: "high".to_owned(),
            },
        ];
        let first = select_label_indices(&candidates, 1, view);
        candidates.swap(0, 1);
        let second = select_label_indices(&candidates, 1, view);
        assert_eq!(candidates[second[0]].stable_key, 2);
        assert_eq!(first.len(), second.len());
        let overlap = select_label_indices(&candidates, 2, view);
        assert_eq!(overlap.len(), 1);
        assert_eq!(candidates[overlap[0]].stable_key, 2);
    }

    #[test]
    fn portrait_horizontal_edge_intersection_remains_visible() {
        let view = RenderView {
            width: 100,
            height: 200,
            ..RenderView::default()
        };
        // The center is at x=2.1 NDC, while the world-space radius expands
        // to 1.2 NDC horizontally in this portrait view.
        assert!(circle_visible(RenderPoint::new(1.05, 0.0), 0.6, view));
    }

    #[test]
    fn glyph_geometry_is_clip_space_and_not_a_solid_bar() {
        let view = RenderView {
            width: 200,
            height: 100,
            center: RenderPoint::new(1.0, -0.5),
            zoom: 2.0,
            rotation_radians: 0.4,
        };
        let candidate = LabelCandidate {
            stable_key: 1,
            position: RenderPoint::new(1.25, -0.25),
            width: 0.1,
            priority: 0,
            visibility: 1.0,
            opacity: 1.0,
            color: [255, 255, 255],
            text: "A".to_owned(),
        };
        let mut vertices = Vec::new();
        assert_eq!(append_label_glyphs(&mut vertices, &candidate, view, 1), 1);
        assert_eq!(vertices.len(), label_vertex_count(&candidate.text, 1));
        assert!(vertices.len() > 6);
        let point = world_to_ndc(candidate.position, view);
        assert!(
            vertices
                .iter()
                .all(|vertex| { vertex.position[0].is_finite() && vertex.position[1].is_finite() })
        );
        assert!(
            vertices
                .iter()
                .any(|vertex| (vertex.position[0] - point[0]).abs() > 0.001)
        );
        assert!(
            vertices
                .windows(2)
                .any(|pair| (pair[0].position[1] - pair[1].position[1]).abs() > 0.0001)
        );
    }

    #[test]
    fn unicode_uses_visible_fallback_and_budget_caps_work() {
        let candidate = LabelCandidate {
            stable_key: 7,
            position: RenderPoint::new(0.0, 0.0),
            width: 0.2,
            priority: 0,
            visibility: 1.0,
            opacity: 1.0,
            color: [255, 255, 255],
            text: "é".to_owned(),
        };
        let mut fallback = Vec::new();
        assert_eq!(
            append_label_glyphs(
                &mut fallback,
                &candidate,
                RenderView {
                    width: 100,
                    height: 100,
                    ..RenderView::default()
                },
                1
            ),
            1
        );
        assert!(fallback.len() >= 6);

        let long = LabelCandidate {
            text: "ABCDEFGHIJKLMNOPQRSTUVWXYZ".to_owned(),
            ..candidate
        };
        let mut bounded = Vec::new();
        assert_eq!(
            append_label_glyphs(
                &mut bounded,
                &long,
                RenderView {
                    width: 100,
                    height: 100,
                    ..RenderView::default()
                },
                3
            ),
            3
        );
        assert_eq!(bounded.len(), label_vertex_count(&long.text, 3));
    }
}
