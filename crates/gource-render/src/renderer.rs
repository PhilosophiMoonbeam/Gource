// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Batched WGSL rendering for prepared simulation snapshots.
//!
//! This module owns GPU resources but not a surface, window, command encoder,
//! or submission.  The caller supplies the target view and encoder and decides
//! when to submit the resulting command buffer.

use bytemuck::{Pod, Zeroable};
use gource_core::Catalog;
use gource_sim::{LabelVisual, SceneSnapshot, Vec2};
use std::borrow::Cow;
use std::mem::size_of;
use thiserror::Error;

use crate::gpu::GpuContext;
use crate::scene::{
    LABEL_GLYPH_ADVANCE_WORLD, LabelCandidate, LabelSelectionScratch, LabelVertex,
    MAX_LABEL_LAYOUT_GLYPHS, NodeInstance, PremultipliedColor, PreparedScene, QuadVertex,
    RenderPoint, RenderView, RendererConfig, ResourceLimitError, TriangleVertex,
    append_action_triangle, append_label_glyphs, append_segment, checked_buffer_bytes,
    circle_visible, grown_capacity, label_glyph_count, label_vertex_count,
    select_label_indices_into,
};

/// Canonical scene target.  The presenter owns any later surface conversion.
pub const RENDER_TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const TARGET_FORMAT: wgpu::TextureFormat = RENDER_TARGET_FORMAT;
const NODE_QUAD: [QuadVertex; 6] = [
    QuadVertex {
        corner: [-1.0, -1.0],
    },
    QuadVertex {
        corner: [-1.0, 1.0],
    },
    QuadVertex { corner: [1.0, 1.0] },
    QuadVertex {
        corner: [-1.0, -1.0],
    },
    QuadVertex { corner: [1.0, 1.0] },
    QuadVertex {
        corner: [1.0, -1.0],
    },
];

/// A supplied target view and the metadata required for validation.
pub struct RenderTarget<'a> {
    pub view: &'a wgpu::TextureView,
    pub width: u32,
    pub height: u32,
    pub format: wgpu::TextureFormat,
    pub sample_count: u32,
}

impl<'a> RenderTarget<'a> {
    #[must_use]
    pub fn new(
        view: &'a wgpu::TextureView,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
        sample_count: u32,
    ) -> Self {
        Self {
            view,
            width,
            height,
            format,
            sample_count,
        }
    }

    pub fn validate(&self) -> Result<(), RendererError> {
        validate_target_metadata(self.width, self.height, self.format, self.sample_count)
    }
}

fn validate_target_metadata(
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    sample_count: u32,
) -> Result<(), RendererError> {
    if width == 0 || height == 0 {
        return Err(RendererError::ZeroExtent { width, height });
    }
    if format != TARGET_FORMAT {
        return Err(RendererError::UnsupportedTargetFormat {
            expected: TARGET_FORMAT,
            actual: format,
        });
    }
    if sample_count != 1 {
        return Err(RendererError::UnsupportedSampleCount {
            expected: 1,
            actual: sample_count,
        });
    }
    Ok(())
}
/// Counts and byte measurements from one encoded scene.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderStats {
    pub branch_vertices: usize,
    pub action_vertices: usize,
    pub node_instances: usize,
    pub contributor_instances: usize,
    pub labels_considered: usize,
    pub labels_selected: usize,
    pub label_glyphs: usize,
    pub label_vertices: usize,
    pub culled_objects: usize,
    pub bytes_uploaded: u64,
    pub draw_calls: u32,
}

impl RenderStats {
    #[must_use]
    pub fn total_vertices(self) -> usize {
        self.branch_vertices + self.action_vertices + self.label_vertices
    }
}

/// Errors encountered while preparing or encoding a scene.
#[derive(Debug, Error)]
pub enum RendererError {
    #[error(
        "render target extent {width}x{height} is invalid; width and height must both be non-zero"
    )]
    ZeroExtent { width: u32, height: u32 },
    #[error(
        "render target format {actual:?} is unsupported; renderer requires {expected:?} (RGBA8Unorm, not sRGB)"
    )]
    UnsupportedTargetFormat {
        expected: wgpu::TextureFormat,
        actual: wgpu::TextureFormat,
    },
    #[error(
        "render target sample count {actual} is unsupported; renderer pipelines require exactly {expected} sample"
    )]
    UnsupportedSampleCount { expected: u32, actual: u32 },
    #[error(
        "renderer needs {required} vertex buffers but adapter exposes only {available}; use a WebGPU-capable adapter"
    )]
    UnsupportedCapabilities { required: u32, available: u32 },
    #[error(
        "renderer needs {required} vertex attributes but adapter exposes only {available}; use a WebGPU-capable adapter"
    )]
    UnsupportedVertexAttributes { required: u32, available: u32 },
    #[error("renderer uniform buffer limit {available} is below required {required}")]
    UnsupportedUniformLimit { required: u64, available: u64 },
    #[error(transparent)]
    ResourceLimit(#[from] ResourceLimitError),
    #[error(
        "GPU buffer allocation for {resource} requires {requested} bytes, above device limit {maximum}"
    )]
    DeviceBufferLimit {
        resource: &'static str,
        requested: u64,
        maximum: u64,
    },
    #[error("GPU buffer mapping failed for {resource}: {source}")]
    BufferMapping {
        resource: &'static str,
        #[source]
        source: wgpu::MapRangeError,
    },
    #[error("scene visual data is invalid: {0}")]
    InvalidScene(&'static str),
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct GlobalsUniform {
    // Column-major matrix.  It includes aspect correction, camera rotation,
    // and scale/translation, with explicit four-float columns.
    transform: [[f32; 4]; 4],
}

/// Batching renderer for immutable [`SceneSnapshot`] values.
struct UploadedBuffers {
    globals_bind_group: wgpu::BindGroup,
    branch: Option<wgpu::Buffer>,
    action: Option<wgpu::Buffer>,
    nodes: Option<wgpu::Buffer>,
    contributors: Option<wgpu::Buffer>,
    labels: Option<wgpu::Buffer>,
}

/// Batching renderer for immutable [`SceneSnapshot`] values.
pub struct SceneRenderer {
    device: wgpu::Device,
    config: RendererConfig,
    globals_layout: wgpu::BindGroupLayout,
    branch_pipeline: wgpu::RenderPipeline,
    action_pipeline: wgpu::RenderPipeline,
    node_pipeline: wgpu::RenderPipeline,
    label_pipeline: wgpu::RenderPipeline,
    // CPU geometry and label-selection storage are retained across frames.
    // GPU upload buffers remain per-encode resources: queue writes to shared
    // buffers would alias when callers record multiple passes before one
    // submission.
    prepared: PreparedScene,
    label_candidates: Vec<LabelCandidate>,
    label_selection: LabelSelectionScratch,
    quad_buffer: wgpu::Buffer,
}

impl std::fmt::Debug for SceneRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SceneRenderer")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl SceneRenderer {
    /// Create pipelines and reusable buffers from a caller-owned context.
    pub fn new(context: &GpuContext, config: RendererConfig) -> Result<Self, RendererError> {
        Self::from_device_queue(context.device(), context.queue(), config)
    }

    /// Create a renderer from caller-created device and queue handles.
    pub fn from_device_queue(
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        config: RendererConfig,
    ) -> Result<Self, RendererError> {
        config
            .view
            .validate()
            .map_err(|error| RendererError::ZeroExtent {
                width: error.width,
                height: error.height,
            })?;
        let limits = device.limits();
        if limits.max_vertex_buffers < 2 {
            return Err(RendererError::UnsupportedCapabilities {
                required: 2,
                available: limits.max_vertex_buffers,
            });
        }
        if limits.max_vertex_attributes < 6 {
            return Err(RendererError::UnsupportedVertexAttributes {
                required: 6,
                available: limits.max_vertex_attributes,
            });
        }
        if limits.max_uniform_buffer_binding_size < size_of::<GlobalsUniform>() as u64 {
            return Err(RendererError::UnsupportedUniformLimit {
                required: size_of::<GlobalsUniform>() as u64,
                available: limits.max_uniform_buffer_binding_size,
            });
        }

        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gource-render-globals-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(size_of::<GlobalsUniform>() as u64),
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("gource-render-pipeline-layout"),
            bind_group_layouts: &[Some(&globals_layout)],
            immediate_size: 0,
        });

        let triangle_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gource-render-triangle-wgsl"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(TRIANGLE_WGSL)),
        });
        let node_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gource-render-node-wgsl"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(NODE_WGSL)),
        });
        let label_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gource-render-label-wgsl"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(LABEL_WGSL)),
        });
        let branch_pipeline = create_triangle_pipeline(
            device,
            &pipeline_layout,
            &triangle_shader,
            "gource-render-branches",
        );
        let action_pipeline = create_triangle_pipeline(
            device,
            &pipeline_layout,
            &triangle_shader,
            "gource-render-actions",
        );
        let label_pipeline = create_triangle_pipeline(
            device,
            &pipeline_layout,
            &label_shader,
            "gource-render-labels",
        );
        let node_pipeline = create_node_pipeline(device, &pipeline_layout, &node_shader);
        let quad_size = size_of::<QuadVertex>() as u64 * NODE_QUAD.len() as u64;
        let quad_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gource-render-node-quad"),
            size: quad_size,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: true,
        });
        let mut quad_view = quad_buffer
            .slice(..)
            .get_mapped_range_mut()
            .map_err(|source| RendererError::BufferMapping {
                resource: "gource-render-node-quad",
                source,
            })?;
        quad_view.copy_from_slice(bytemuck::cast_slice(&NODE_QUAD));
        drop(quad_view);
        quad_buffer.unmap();

        Ok(Self {
            device: device.clone(),
            config,
            globals_layout,
            branch_pipeline,
            action_pipeline,
            node_pipeline,
            label_pipeline,
            prepared: PreparedScene::default(),
            label_candidates: Vec::new(),
            label_selection: LabelSelectionScratch::default(),
            quad_buffer,
        })
    }

    /// Update the camera/viewport without recreating pipelines or buffers.
    pub fn set_view(&mut self, view: RenderView) -> Result<(), RendererError> {
        view.validate().map_err(|error| RendererError::ZeroExtent {
            width: error.width,
            height: error.height,
        })?;
        self.config.view = view;
        Ok(())
    }

    /// Current camera and target metadata.
    #[must_use]
    pub fn view(&self) -> RenderView {
        self.config.view
    }

    /// Prepare CPU geometry for a snapshot.  No GPU work is performed here.
    pub fn prepare_scene(
        &self,
        snapshot: &SceneSnapshot,
        catalog: &Catalog,
    ) -> Result<PreparedScene, RendererError> {
        self.prepare_scene_with_view(snapshot, catalog, self.config.view)
    }

    /// Encode one scene into a target using the configured viewport.
    ///
    /// This method only records commands.  It never submits the encoder and
    /// never touches a surface.
    pub fn render(
        &mut self,
        snapshot: &SceneSnapshot,
        catalog: &Catalog,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
    ) -> Result<RenderStats, RendererError> {
        let view = self.config.view;
        self.render_to_target(
            snapshot,
            catalog,
            encoder,
            RenderTarget::new(target, view.width, view.height, TARGET_FORMAT, 1),
        )
    }

    /// Encode one scene into a fully-described supplied target.
    pub fn render_to_target(
        &mut self,
        snapshot: &SceneSnapshot,
        catalog: &Catalog,
        encoder: &mut wgpu::CommandEncoder,
        target: RenderTarget<'_>,
    ) -> Result<RenderStats, RendererError> {
        target.validate()?;
        let mut view = self.config.view;
        view.width = target.width;
        view.height = target.height;
        self.prepare_scene_reusing(snapshot, catalog, view)?;
        let prepared = &self.prepared;
        let uploads = Self::upload(
            &self.device,
            &self.globals_layout,
            self.config.resource_limits,
            prepared,
            globals_for_view(view),
            encoder,
        )?;

        let mut stats = RenderStats {
            branch_vertices: prepared.branch_vertices.len(),
            action_vertices: prepared.action_vertices.len(),
            node_instances: prepared.node_instances.len(),
            contributor_instances: prepared.contributor_instances.len(),
            labels_considered: snapshot.labels.len(),
            labels_selected: prepared.visible_labels,
            label_glyphs: prepared.label_glyphs,
            label_vertices: prepared.label_vertices.len(),
            culled_objects: prepared.culled_objects,
            bytes_uploaded: 0,
            draw_calls: 0,
        };
        stats.bytes_uploaded = (prepared.branch_vertices.len() * size_of::<TriangleVertex>()
            + prepared.action_vertices.len() * size_of::<TriangleVertex>()
            + prepared.node_instances.len() * size_of::<NodeInstance>()
            + prepared.contributor_instances.len() * size_of::<NodeInstance>()
            + prepared.label_vertices.len() * size_of::<LabelVertex>())
            as u64;

        let clear = wgpu::Color {
            r: self.config.clear_color[0].clamp(0.0, 1.0),
            g: self.config.clear_color[1].clamp(0.0, 1.0),
            b: self.config.clear_color[2].clamp(0.0, 1.0),
            a: self.config.clear_color[3].clamp(0.0, 1.0),
        };
        let color_attachment = wgpu::RenderPassColorAttachment {
            view: target.view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(clear),
                store: wgpu::StoreOp::Store,
            },
        };
        let color_attachments = [Some(color_attachment)];
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("gource-render-scene"),
            color_attachments: &color_attachments,
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_bind_group(0, &uploads.globals_bind_group, &[]);
        if let Some(buffer) = uploads.branch.as_ref()
            && !prepared.branch_vertices.is_empty()
        {
            pass.set_pipeline(&self.branch_pipeline);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..prepared.branch_vertices.len() as u32, 0..1);
            stats.draw_calls += 1;
        }
        if let Some(buffer) = uploads.action.as_ref()
            && !prepared.action_vertices.is_empty()
        {
            pass.set_pipeline(&self.action_pipeline);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..prepared.action_vertices.len() as u32, 0..1);
            stats.draw_calls += 1;
        }
        if let Some(nodes) = uploads.nodes.as_ref()
            && !prepared.node_instances.is_empty()
        {
            pass.set_pipeline(&self.node_pipeline);
            pass.set_vertex_buffer(0, self.quad_buffer.slice(..));
            pass.set_vertex_buffer(1, nodes.slice(..));
            pass.draw(
                0..NODE_QUAD.len() as u32,
                0..prepared.node_instances.len() as u32,
            );
            stats.draw_calls += 1;
        }
        if let Some(contributors) = uploads.contributors.as_ref()
            && !prepared.contributor_instances.is_empty()
        {
            pass.set_pipeline(&self.node_pipeline);
            pass.set_vertex_buffer(0, self.quad_buffer.slice(..));
            pass.set_vertex_buffer(1, contributors.slice(..));
            pass.draw(
                0..NODE_QUAD.len() as u32,
                0..prepared.contributor_instances.len() as u32,
            );
            stats.draw_calls += 1;
        }
        if let Some(buffer) = uploads.labels.as_ref()
            && !prepared.label_vertices.is_empty()
        {
            pass.set_pipeline(&self.label_pipeline);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..prepared.label_vertices.len() as u32, 0..1);
            stats.draw_calls += 1;
        }
        drop(pass);
        Ok(stats)
    }

    fn prepare_scene_with_view(
        &self,
        snapshot: &SceneSnapshot,
        catalog: &Catalog,
        view: RenderView,
    ) -> Result<PreparedScene, RendererError> {
        // A local renderer view avoids mutating the caller's configured size
        // when an offscreen target changes between frames.
        let clone = SelfViewConfig {
            resource_limits: self.config.resource_limits,
            label_budget: self.config.label_budget,
        };
        let mut prepared = PreparedScene::default();
        let mut label_candidates = Vec::new();
        let mut label_selection = LabelSelectionScratch::default();
        prepare_scene_for_view(
            snapshot,
            catalog,
            view,
            &clone,
            &mut prepared,
            &mut label_candidates,
            &mut label_selection,
        )?;
        Ok(prepared)
    }

    fn prepare_scene_reusing(
        &mut self,
        snapshot: &SceneSnapshot,
        catalog: &Catalog,
        view: RenderView,
    ) -> Result<(), RendererError> {
        // A local renderer view avoids mutating the caller's configured size
        // when an offscreen target changes between frames.
        let clone = SelfViewConfig {
            resource_limits: self.config.resource_limits,
            label_budget: self.config.label_budget,
        };
        prepare_scene_for_view(
            snapshot,
            catalog,
            view,
            &clone,
            &mut self.prepared,
            &mut self.label_candidates,
            &mut self.label_selection,
        )
    }

    fn upload(
        device: &wgpu::Device,
        globals_layout: &wgpu::BindGroupLayout,
        limits: crate::scene::ResourceLimits,
        prepared: &PreparedScene,
        globals: GlobalsUniform,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Result<UploadedBuffers, RendererError> {
        let device_max_bytes = device.limits().max_buffer_size;
        let globals_buffer = upload_values(
            device,
            encoder,
            "gource-render-globals",
            std::slice::from_ref(&globals),
            1,
            device_max_bytes,
            wgpu::BufferUsages::UNIFORM,
        )?
        .ok_or(RendererError::InvalidScene(
            "global uniform upload is empty",
        ))?;
        let globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("gource-render-globals"),
            layout: globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            }],
        });

        let branch = upload_values(
            device,
            encoder,
            "gource-render-branches",
            &prepared.branch_vertices,
            limits.max_triangle_vertices,
            limits.max_buffer_bytes,
            wgpu::BufferUsages::VERTEX,
        )?;

        let action = upload_values(
            device,
            encoder,
            "gource-render-actions",
            &prepared.action_vertices,
            limits.max_triangle_vertices,
            limits.max_buffer_bytes,
            wgpu::BufferUsages::VERTEX,
        )?;

        let nodes = upload_values(
            device,
            encoder,
            "gource-render-nodes",
            &prepared.node_instances,
            limits.max_node_instances,
            limits.max_buffer_bytes,
            wgpu::BufferUsages::VERTEX,
        )?;

        let contributors = upload_values(
            device,
            encoder,
            "gource-render-contributors",
            &prepared.contributor_instances,
            limits.max_node_instances,
            limits.max_buffer_bytes,
            wgpu::BufferUsages::VERTEX,
        )?;

        let labels = upload_values(
            device,
            encoder,
            "gource-render-labels",
            &prepared.label_vertices,
            limits.max_label_vertices,
            limits.max_buffer_bytes,
            wgpu::BufferUsages::VERTEX,
        )?;

        Ok(UploadedBuffers {
            globals_bind_group,
            branch,
            action,
            nodes,
            contributors,
            labels,
        })
    }
}

fn upload_values<T: Pod>(
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    resource: &'static str,
    values: &[T],
    max_items: usize,
    max_bytes: u64,
    destination_usage: wgpu::BufferUsages,
) -> Result<Option<wgpu::Buffer>, RendererError> {
    let bytes = checked_buffer_bytes(resource, values.len(), size_of::<T>(), max_items, max_bytes)?;
    if values.is_empty() {
        return Ok(None);
    }
    let device_max_bytes = device.limits().max_buffer_size;
    if bytes > device_max_bytes {
        return Err(RendererError::DeviceBufferLimit {
            resource,
            requested: bytes,
            maximum: device_max_bytes,
        });
    }
    let byte_len = usize::try_from(bytes).map_err(|_| RendererError::DeviceBufferLimit {
        resource,
        requested: bytes,
        maximum: usize::MAX as u64,
    })?;
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(resource),
        size: bytes,
        usage: wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: true,
    });
    let mut staging_view = staging
        .slice(..)
        .get_mapped_range_mut()
        .map_err(|source| RendererError::BufferMapping { resource, source })?;
    staging_view.copy_from_slice(&bytemuck::cast_slice(values)[..byte_len]);
    drop(staging_view);
    staging.unmap();

    let destination = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(resource),
        size: bytes,
        usage: destination_usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    encoder.copy_buffer_to_buffer(&staging, 0, &destination, 0, bytes);
    Ok(Some(destination))
}

#[derive(Clone, Copy)]
struct SelfViewConfig {
    resource_limits: crate::scene::ResourceLimits,
    label_budget: usize,
}

fn prepare_scene_for_view(
    snapshot: &SceneSnapshot,
    catalog: &Catalog,
    view: RenderView,
    config: &SelfViewConfig,
    prepared: &mut PreparedScene,
    label_candidates: &mut Vec<LabelCandidate>,
    label_selection: &mut LabelSelectionScratch,
) -> Result<(), RendererError> {
    view.validate().map_err(|error| RendererError::ZeroExtent {
        width: error.width,
        height: error.height,
    })?;
    let limits = config.resource_limits;
    let node_count = snapshot
        .directories
        .len()
        .saturating_add(snapshot.files.len());
    if snapshot.branches.len().saturating_mul(6) > limits.max_triangle_vertices {
        return Err(RendererError::ResourceLimit(ResourceLimitError {
            resource: "branches",
            requested: snapshot.branches.len().saturating_mul(6),
            maximum: limits.max_triangle_vertices,
        }));
    }
    if snapshot.actions.len().saturating_mul(3) > limits.max_triangle_vertices {
        return Err(RendererError::ResourceLimit(ResourceLimitError {
            resource: "actions",
            requested: snapshot.actions.len().saturating_mul(3),
            maximum: limits.max_triangle_vertices,
        }));
    }
    if node_count > limits.max_node_instances {
        return Err(RendererError::ResourceLimit(ResourceLimitError {
            resource: "nodes",
            requested: node_count,
            maximum: limits.max_node_instances,
        }));
    }
    if snapshot.contributors.len() > limits.max_node_instances {
        return Err(RendererError::ResourceLimit(ResourceLimitError {
            resource: "contributors",
            requested: snapshot.contributors.len(),
            maximum: limits.max_node_instances,
        }));
    }
    if snapshot.labels.len() > limits.max_labels {
        return Err(RendererError::ResourceLimit(ResourceLimitError {
            resource: "labels",
            requested: snapshot.labels.len(),
            maximum: limits.max_labels,
        }));
    }

    prepared.clear();

    for branch in &snapshot.branches {
        let start = point(branch.start);
        let end = point(branch.end);
        let midpoint = RenderPoint::new((start.x + end.x) * 0.5, (start.y + end.y) * 0.5);
        let span = distance(start, end);
        if !circle_visible(midpoint, span * 0.5 + 0.01, view) {
            prepared.culled_objects = prepared.culled_objects.saturating_add(1);
            continue;
        }
        reserve_geometry(
            &mut prepared.branch_vertices,
            6,
            limits.max_triangle_vertices,
            limits.max_buffer_bytes,
            "branches",
        )?;
        append_segment(
            &mut prepared.branch_vertices,
            start,
            end,
            0.008,
            color_with_opacity(branch.color, branch.opacity),
        );
    }
    for action in &snapshot.actions {
        let position = point(action.position);
        let target = point(action.target);
        if !circle_visible(position, 0.025, view) && !circle_visible(target, 0.025, view) {
            prepared.culled_objects = prepared.culled_objects.saturating_add(1);
            continue;
        }
        reserve_geometry(
            &mut prepared.action_vertices,
            3,
            limits.max_triangle_vertices,
            limits.max_buffer_bytes,
            "actions",
        )?;
        append_action_triangle(
            &mut prepared.action_vertices,
            position,
            RenderPoint::new(target.x - position.x, target.y - position.y),
            0.014,
            color_with_opacity(action.color, action.opacity),
        );
    }
    for directory in &snapshot.directories {
        if !circle_visible(point(directory.position), directory.radius, view) {
            prepared.culled_objects = prepared.culled_objects.saturating_add(1);
            continue;
        }
        reserve_geometry(
            &mut prepared.node_instances,
            1,
            limits.max_node_instances,
            limits.max_buffer_bytes,
            "nodes",
        )?;
        prepared.node_instances.push(NodeInstance {
            center: [directory.position.x, directory.position.y],
            radius: directory.radius.abs().max(0.002),
            opacity: finite_opacity(directory.opacity),
            color: color_with_opacity(directory.color, directory.opacity),
            kind: 0,
            _padding: [0; 3],
        });
    }
    for file in &snapshot.files {
        if !circle_visible(point(file.position), file.radius, view) {
            prepared.culled_objects = prepared.culled_objects.saturating_add(1);
            continue;
        }
        reserve_geometry(
            &mut prepared.node_instances,
            1,
            limits.max_node_instances,
            limits.max_buffer_bytes,
            "nodes",
        )?;
        prepared.node_instances.push(NodeInstance {
            center: [file.position.x, file.position.y],
            radius: file.radius.abs().max(0.0015),
            opacity: finite_opacity(file.opacity),
            color: color_with_opacity(file.color, file.opacity),
            kind: 1,
            _padding: [0; 3],
        });
    }
    for contributor in &snapshot.contributors {
        let energy = finite_opacity(contributor.energy);
        if !circle_visible(point(contributor.position), 0.018 + energy * 0.018, view) {
            prepared.culled_objects = prepared.culled_objects.saturating_add(1);
            continue;
        }
        reserve_geometry(
            &mut prepared.contributor_instances,
            1,
            limits.max_node_instances,
            limits.max_buffer_bytes,
            "contributors",
        )?;
        prepared.contributor_instances.push(NodeInstance {
            center: [contributor.position.x, contributor.position.y],
            radius: 0.018 + energy * 0.018,
            opacity: 0.35 + energy * 0.65,
            color: color_with_opacity(contributor.color, 0.35 + energy * 0.65),
            kind: 2,
            _padding: [0; 3],
        });
    }

    let mut candidate_count = 0usize;
    for label in snapshot.labels.iter().filter(|label| label.visible) {
        if candidate_count == label_candidates.len() {
            reserve_items(
                label_candidates,
                candidate_count.saturating_add(1),
                limits.max_labels,
                "labels",
            )?;
            label_candidates.push(label_candidate(label, catalog));
        } else {
            fill_label_candidate(&mut label_candidates[candidate_count], label, catalog);
        }
        candidate_count += 1;
    }
    let candidates = &label_candidates[..candidate_count];
    let selected =
        select_label_indices_into(candidates, config.label_budget, view, label_selection);
    let mut remaining_glyphs = limits.max_label_glyphs;
    for &candidate_index in label_selection.selected().iter().take(selected) {
        if remaining_glyphs == 0 {
            break;
        }
        let candidate = &candidates[candidate_index];
        let mut glyphs = label_glyph_count(&candidate.text, remaining_glyphs);
        let available_vertices = limits
            .max_label_vertices
            .saturating_sub(prepared.label_vertices.len());
        while glyphs > 0 && label_vertex_count(&candidate.text, glyphs) > available_vertices {
            glyphs -= 1;
        }
        if glyphs == 0 {
            continue;
        }
        let vertices = label_vertex_count(&candidate.text, glyphs);
        reserve_geometry(
            &mut prepared.label_vertices,
            vertices,
            limits.max_label_vertices,
            limits.max_buffer_bytes,
            "label vertices",
        )?;
        let before = prepared.label_vertices.len();
        let emitted = append_label_glyphs(&mut prepared.label_vertices, candidate, view, glyphs);
        let emitted_vertices = prepared.label_vertices.len().saturating_sub(before);
        if emitted == 0 || emitted_vertices == 0 {
            continue;
        }
        prepared.label_glyphs = prepared.label_glyphs.saturating_add(emitted);
        prepared.visible_labels = prepared.visible_labels.saturating_add(1);
        remaining_glyphs = remaining_glyphs.saturating_sub(emitted);
    }
    Ok(())
}

fn label_candidate(label: &LabelVisual, catalog: &Catalog) -> LabelCandidate {
    let mut candidate = LabelCandidate {
        stable_key: 0,
        position: RenderPoint::default(),
        width: 0.0,
        priority: 0,
        visibility: 0.0,
        opacity: 0.0,
        color: [0; 3],
        text: String::new(),
    };
    fill_label_candidate(&mut candidate, label, catalog);
    candidate
}

fn fill_label_candidate(candidate: &mut LabelCandidate, label: &LabelVisual, catalog: &Catalog) {
    let source = if label.text.is_empty() {
        catalog
            .path(label.target)
            .map(|path| path.canonical())
            .unwrap_or("")
    } else {
        label.text.as_str()
    };
    // Bound the renderer-owned copy even if an upstream source contains a
    // malformed or unexpectedly long Unicode string.  Reusing the String
    // keeps its allocation alive when labels are refreshed in place.
    candidate.text.clear();
    candidate
        .text
        .extend(source.chars().take(MAX_LABEL_LAYOUT_GLYPHS));
    let glyph_count = label_glyph_count(&candidate.text, MAX_LABEL_LAYOUT_GLYPHS).max(1);
    candidate.stable_key = label.target.as_u64();
    candidate.position = point(label.position);
    candidate.width = (glyph_count as f32 * LABEL_GLYPH_ADVANCE_WORLD).clamp(0.036, 0.9);
    candidate.priority = u32::from(label.priority);
    candidate.visibility = 1.0;
    candidate.opacity = label.opacity();
    candidate.color = label_color(label.target.as_u64());
}

fn label_color(stable_key: u64) -> [u8; 3] {
    // Bright, restrained colors remain legible against the default dark clear
    // color while making absent custom label colors deterministic per target.
    const PALETTE: [[u8; 3]; 6] = [
        [236, 243, 255],
        [255, 218, 145],
        [156, 224, 255],
        [205, 190, 255],
        [168, 239, 191],
        [255, 181, 204],
    ];
    let mixed = stable_key
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .rotate_left(17);
    PALETTE[(mixed as usize) % PALETTE.len()]
}

fn reserve_items<T>(
    output: &mut Vec<T>,
    requested: usize,
    maximum: usize,
    resource: &'static str,
) -> Result<(), RendererError> {
    if requested > maximum {
        return Err(RendererError::ResourceLimit(ResourceLimitError {
            resource,
            requested,
            maximum,
        }));
    }
    if requested <= output.capacity() {
        return Ok(());
    }
    let target = grown_capacity(output.capacity(), requested, maximum)?;
    output.reserve_exact(target - output.capacity());
    Ok(())
}

fn reserve_geometry<T>(
    output: &mut Vec<T>,
    additional: usize,
    maximum: usize,
    max_bytes: u64,
    resource: &'static str,
) -> Result<(), RendererError> {
    let requested = output
        .len()
        .checked_add(additional)
        .ok_or(RendererError::ResourceLimit(ResourceLimitError {
            resource,
            requested: usize::MAX,
            maximum,
        }))?;
    checked_buffer_bytes(resource, requested, size_of::<T>(), maximum, max_bytes)?;
    if requested <= output.capacity() {
        return Ok(());
    }
    let item_size = size_of::<T>() as u64;
    let byte_limited_items = usize::try_from(max_bytes / item_size).unwrap_or(usize::MAX);
    let capacity_limit = maximum.min(byte_limited_items);
    let target = grown_capacity(output.capacity(), requested, capacity_limit)?;
    checked_buffer_bytes(resource, target, size_of::<T>(), maximum, max_bytes)?;
    output.reserve_exact(target - output.capacity());
    Ok(())
}

fn point(value: Vec2) -> RenderPoint {
    RenderPoint::new(value.x, value.y).finite()
}

fn distance(start: RenderPoint, end: RenderPoint) -> f32 {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    (dx * dx + dy * dy).sqrt()
}

fn finite_opacity(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn color_with_opacity(color: gource_core::Rgb8, opacity: f32) -> [f32; 4] {
    PremultipliedColor::from_rgb8(color.as_array(), finite_opacity(opacity)).0
}

fn globals_for_view(view: RenderView) -> GlobalsUniform {
    let angle = if view.rotation_radians.is_finite() {
        view.rotation_radians
    } else {
        0.0
    };
    let (sin, cos) = angle.sin_cos();
    let zoom = if view.zoom.is_finite() && view.zoom > 0.0 {
        view.zoom
    } else {
        1.0
    };
    let aspect = view.aspect().max(f32::MIN_POSITIVE);
    // Matrix maps world coordinates directly to clip coordinates.  The camera
    // center is applied in the matrix so all batches share one uniform.
    let sx = zoom / aspect;
    let sy = zoom;
    GlobalsUniform {
        transform: [
            [cos * sx, sin * sy, 0.0, 0.0],
            [-sin * sx, cos * sy, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [
                -view.center.x * cos * sx + view.center.y * sin * sx,
                -view.center.x * sin * sy - view.center.y * cos * sy,
                0.0,
                1.0,
            ],
        ],
    }
}

const PREMULTIPLIED_BLEND: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    },
};

fn create_triangle_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    label: &'static str,
) -> wgpu::RenderPipeline {
    const ATTRIBUTES: &[wgpu::VertexAttribute] =
        &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4];
    let vertex_layout = wgpu::VertexBufferLayout {
        array_stride: size_of::<TriangleVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: ATTRIBUTES,
    };
    let target = wgpu::ColorTargetState {
        format: TARGET_FORMAT,
        blend: Some(PREMULTIPLIED_BLEND),
        write_mask: wgpu::ColorWrites::ALL,
    };
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[Some(vertex_layout)],
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(target)],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn create_node_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
) -> wgpu::RenderPipeline {
    const QUAD_ATTRIBUTES: &[wgpu::VertexAttribute] = &wgpu::vertex_attr_array![0 => Float32x2];
    const INSTANCE_ATTRIBUTES: &[wgpu::VertexAttribute] = &wgpu::vertex_attr_array![
        1 => Float32x2,
        2 => Float32,
        3 => Float32,
        4 => Float32x4,
        5 => Uint32,
    ];
    let vertex_layout = wgpu::VertexBufferLayout {
        array_stride: size_of::<QuadVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: QUAD_ATTRIBUTES,
    };
    let instance_layout = wgpu::VertexBufferLayout {
        array_stride: size_of::<NodeInstance>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: INSTANCE_ATTRIBUTES,
    };
    let target = wgpu::ColorTargetState {
        format: TARGET_FORMAT,
        blend: Some(PREMULTIPLIED_BLEND),
        write_mask: wgpu::ColorWrites::ALL,
    };
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("gource-render-nodes"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[Some(vertex_layout), Some(instance_layout)],
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(target)],
        }),
        multiview_mask: None,
        cache: None,
    })
}

const TRIANGLE_WGSL: &str = r#"
// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later
struct Globals { transform: mat4x4<f32> };
@group(0) @binding(0) var<uniform> globals: Globals;
struct VertexOut { @builtin(position) position: vec4<f32>, @location(0) color: vec4<f32> };
@vertex fn vs_main(@location(0) position: vec2<f32>, @location(1) color: vec4<f32>) -> VertexOut {
    var out: VertexOut;
    out.position = globals.transform * vec4<f32>(position, 0.0, 1.0);
    out.color = color;
    return out;
}
@fragment fn fs_main(@location(0) color: vec4<f32>) -> @location(0) vec4<f32> { return color; }
"#;

const LABEL_WGSL: &str = r#"
// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later
// Label glyph vertices are prepared directly in clip space by
// append_label_glyphs; one pipeline draw batches all selected labels.
struct VertexOut { @builtin(position) position: vec4<f32>, @location(0) color: vec4<f32> };
@vertex fn vs_main(@location(0) position: vec2<f32>, @location(1) color: vec4<f32>) -> VertexOut {
    var out: VertexOut;
    out.position = vec4<f32>(position, 0.0, 1.0);
    out.color = color;
    return out;
}
@fragment fn fs_main(@location(0) color: vec4<f32>) -> @location(0) vec4<f32> { return color; }
"#;

const NODE_WGSL: &str = r#"
// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later
struct Globals { transform: mat4x4<f32> };
@group(0) @binding(0) var<uniform> globals: Globals;
struct VertexOut { @builtin(position) position: vec4<f32>, @location(0) color: vec4<f32> };
@vertex fn vs_main(
    @location(0) corner: vec2<f32>,
    @location(1) center: vec2<f32>,
    @location(2) radius: f32,
    @location(3) opacity: f32,
    @location(4) color: vec4<f32>,
    @location(5) kind: u32,
) -> VertexOut {
    var out: VertexOut;
    let shape_scale = select(1.0, 0.78, kind == 0u);
    let world = center + corner * radius * shape_scale;
    out.position = globals.transform * vec4<f32>(world, 0.0, 1.0);
    out.color = color;
    return out;
}
@fragment fn fs_main(@location(0) color: vec4<f32>) -> @location(0) vec4<f32> { return color; }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_metadata_rejects_multisampling_before_encoding() {
        let error = validate_target_metadata(64, 32, TARGET_FORMAT, 4).unwrap_err();
        assert!(matches!(
            error,
            RendererError::UnsupportedSampleCount {
                expected: 1,
                actual: 4
            }
        ));
    }

    #[test]
    fn target_metadata_accepts_single_sample_rgba8() {
        assert!(validate_target_metadata(64, 32, TARGET_FORMAT, 1).is_ok());
    }
}
