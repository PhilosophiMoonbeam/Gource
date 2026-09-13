// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Caller-owned `wgpu` context creation and capability diagnostics.

use std::fmt;

use thiserror::Error;

/// Options used by [`GpuContext::request`].
///
/// The descriptor deliberately contains no surface or window handle.  A
/// caller can use the returned device and queue for either a swapchain target
/// or an offscreen texture.
#[derive(Clone, Debug)]
pub struct GpuContextOptions {
    /// Backends to consider while looking for an adapter.
    pub backends: wgpu::Backends,
    /// Adapter power preference.
    pub power_preference: wgpu::PowerPreference,
    /// Permit a software/fallback adapter when no hardware adapter matches.
    pub force_fallback_adapter: bool,
    /// Features required by the caller.
    pub required_features: wgpu::Features,
    /// Limits required by the caller.  The default is the wgpu default set.
    pub required_limits: wgpu::Limits,
    /// Optional debug label used for the logical device.
    pub device_label: Option<String>,
}

impl Default for GpuContextOptions {
    fn default() -> Self {
        Self {
            backends: wgpu::Backends::all(),
            power_preference: wgpu::PowerPreference::default(),
            force_fallback_adapter: false,
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            device_label: Some("gource-render-device".to_owned()),
        }
    }
}

/// Stable, bounded diagnostics captured at adapter selection time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterDiagnostics {
    /// Full backend-provided adapter information.
    pub info: wgpu::AdapterInfo,
    /// Features exposed by the selected adapter.
    pub features: wgpu::Features,
    /// Limits exposed by the selected adapter.
    pub limits: wgpu::Limits,
}

impl AdapterDiagnostics {
    /// A short actionable summary suitable for an error or startup log.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "adapter '{}' ({:?}, driver {}), max buffer {} bytes, max vertex buffers {}, max vertex attributes {}",
            self.info.name,
            self.info.backend,
            self.info.driver,
            self.limits.max_buffer_size,
            self.limits.max_vertex_buffers,
            self.limits.max_vertex_attributes,
        )
    }
}

/// A device and queue selected by the caller's policy.
///
/// `GpuContext` does not create a surface and never submits command buffers.
/// The owner decides when and where to submit work encoded by the renderer.
pub struct GpuContext {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    diagnostics: AdapterDiagnostics,
}

impl fmt::Debug for GpuContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GpuContext")
            .field("diagnostics", &self.diagnostics)
            .finish_non_exhaustive()
    }
}

/// Context creation and capability failures.
#[derive(Debug, Error)]
pub enum GpuContextError {
    /// No adapter matched the requested backend and power policy.
    #[error(
        "no compatible GPU adapter was found (backends={backends:?}, fallback={force_fallback_adapter}); {hint}"
    )]
    AdapterUnavailable {
        /// Backends requested by the caller.
        backends: wgpu::Backends,
        /// Whether fallback adapters were allowed.
        force_fallback_adapter: bool,
        /// Bounded diagnostic text from wgpu.
        hint: String,
    },
    /// The selected adapter could not provide the requested device.
    #[error("GPU device request failed on {adapter}: {hint}")]
    DeviceRequest {
        /// Adapter summary.
        adapter: String,
        /// Bounded diagnostic text from wgpu.
        hint: String,
    },
    /// A required capability was not present on the selected adapter.
    #[error(
        "GPU capability '{capability}' is unavailable on {adapter}: required {required}, available {available}"
    )]
    UnsupportedCapability {
        /// Capability name.
        capability: &'static str,
        /// Required value.
        required: u64,
        /// Available value.
        available: u64,
        /// Adapter summary.
        adapter: String,
    },
}

impl GpuContext {
    /// Request a headless-compatible device and queue.
    ///
    /// This is async because adapter and device requests are async in wgpu;
    /// it does not create or require an async runtime.  Callers may await it
    /// from their own executor (or use their existing one-shot poll helper).
    pub async fn request(options: GpuContextOptions) -> Result<Self, GpuContextError> {
        let mut instance_descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_descriptor.backends = options.backends;
        let instance = wgpu::Instance::new(instance_descriptor);

        let request_options = wgpu::RequestAdapterOptions {
            power_preference: options.power_preference,
            compatible_surface: None,
            force_fallback_adapter: options.force_fallback_adapter,
            // This trusted renderer keeps the adapter's full limits.
            apply_limit_buckets: false,
        };
        let adapter = instance
            .request_adapter(&request_options)
            .await
            .map_err(|error| GpuContextError::AdapterUnavailable {
                backends: options.backends,
                force_fallback_adapter: options.force_fallback_adapter,
                hint: format!("{error:?}; install a Vulkan/Metal/DX12 driver or enable fallback"),
            })?;

        let info = adapter.get_info();
        let adapter_limits = adapter.limits();
        let diagnostics = AdapterDiagnostics {
            info,
            features: adapter.features(),
            limits: adapter_limits.clone(),
        };
        validate_required_limits(&options.required_limits, &adapter_limits, &diagnostics)?;

        let device_label = options.device_label.as_deref();
        let descriptor = wgpu::DeviceDescriptor {
            label: device_label,
            required_features: options.required_features,
            required_limits: options.required_limits,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            trace: wgpu::Trace::Off,
        };
        let (device, queue) = adapter.request_device(&descriptor).await.map_err(|error| {
            GpuContextError::DeviceRequest {
                adapter: diagnostics.summary(),
                hint: format!("{error:?}; reduce required features/limits"),
            }
        })?;

        Ok(Self {
            instance,
            adapter,
            device,
            queue,
            diagnostics,
        })
    }

    /// Request a context with default options and no display handle.
    pub async fn request_headless() -> Result<Self, GpuContextError> {
        Self::request(GpuContextOptions::default()).await
    }

    /// Construct a context around caller-created wgpu objects.
    ///
    /// This is useful when the application already selected an adapter and
    /// device.  The context keeps the instance and adapter only to preserve
    /// ownership and diagnostics; rendering still never submits work.
    #[must_use]
    pub fn from_parts(
        instance: wgpu::Instance,
        adapter: wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
    ) -> Self {
        let diagnostics = AdapterDiagnostics {
            info: adapter.get_info(),
            features: adapter.features(),
            limits: adapter.limits(),
        };
        Self {
            instance,
            adapter,
            device,
            queue,
            diagnostics,
        }
    }

    /// The instance that owns the adapter.
    #[must_use]
    pub fn instance(&self) -> &wgpu::Instance {
        &self.instance
    }

    /// The selected adapter.
    #[must_use]
    pub fn adapter(&self) -> &wgpu::Adapter {
        &self.adapter
    }

    /// The caller-owned logical device handle.
    #[must_use]
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The caller-owned queue handle.
    #[must_use]
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Adapter/capability diagnostics captured during request.
    #[must_use]
    pub fn diagnostics(&self) -> &AdapterDiagnostics {
        &self.diagnostics
    }

    /// Clone the cheap wgpu handles for a renderer while retaining caller
    /// ownership of the original context.
    #[must_use]
    pub fn handles(&self) -> (&wgpu::Device, &wgpu::Queue) {
        (&self.device, &self.queue)
    }
}

fn validate_required_limits(
    required: &wgpu::Limits,
    available: &wgpu::Limits,
    diagnostics: &AdapterDiagnostics,
) -> Result<(), GpuContextError> {
    let checks = [
        (
            "max_buffer_size",
            required.max_buffer_size,
            available.max_buffer_size,
        ),
        (
            "max_uniform_buffer_binding_size",
            required.max_uniform_buffer_binding_size,
            available.max_uniform_buffer_binding_size,
        ),
        (
            "max_storage_buffer_binding_size",
            required.max_storage_buffer_binding_size,
            available.max_storage_buffer_binding_size,
        ),
        (
            "max_vertex_buffers",
            u64::from(required.max_vertex_buffers),
            u64::from(available.max_vertex_buffers),
        ),
        (
            "max_vertex_attributes",
            u64::from(required.max_vertex_attributes),
            u64::from(available.max_vertex_attributes),
        ),
    ];
    for (capability, required, available) in checks {
        if required > available {
            return Err(GpuContextError::UnsupportedCapability {
                capability,
                required,
                available,
                adapter: diagnostics.summary(),
            });
        }
    }
    Ok(())
}
