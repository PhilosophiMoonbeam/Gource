// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native `wgpu` scene rendering into caller-provided texture views.
//!
//! Rendering consumes prepared core and simulation data and does not parse
//! repository history or own playback semantics.  The caller owns the
//! [`GpuContext`], target texture, command encoder, and eventual submission.

mod gpu;
mod renderer;
mod scene;
pub use renderer::{RENDER_TARGET_FORMAT, RenderStats, RenderTarget, RendererError, SceneRenderer};

pub use gpu::{AdapterDiagnostics, GpuContext, GpuContextError, GpuContextOptions};
pub use scene::{
    LABEL_GLYPH_ADVANCE_WORLD, LABEL_GLYPH_HEIGHT_WORLD, LabelCandidate, LabelVertex, NodeInstance,
    PremultipliedColor, PreparedScene, QuadVertex, RenderExtentError, RenderPoint, RenderView,
    RendererConfig, ResourceLimitError, ResourceLimits, TriangleVertex, append_action_triangle,
    append_label_glyphs, append_segment, checked_buffer_bytes, circle_visible, grown_capacity,
    select_label_indices,
};
