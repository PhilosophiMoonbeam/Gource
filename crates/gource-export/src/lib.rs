// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Headless frame scheduling, readback, image output, and export metadata.
//!
//! Export reuses the core, simulation, and rendering boundaries without
//! depending on a presentation window or interactive UI.  [`ExportJob`] owns
//! one [`gource_sim::ReplaySession`] and samples it at exact schedule ticks;
//! it never runs a second physics implementation.

mod job;
mod manifest;
mod readback;
mod schedule;
mod sink;

pub use gource_core::{Rational, RationalError};
pub use gource_render::{GpuContext, GpuContextError, GpuContextOptions};

pub use job::{ExportError, ExportJob, ExportOutput, ExportReport, ExportSpec};
pub use manifest::{
    BackendIdentity, EXPORT_MANIFEST_VERSION, EncoderIdentity, ExportConfigIdentity,
    ExportManifestV1, InputIdentity, ManifestError, RenderIdentity, ToolchainIdentity,
};
pub use readback::{
    COPY_BYTES_PER_ROW_ALIGNMENT, GpuReadback, ReadbackError, ReadbackLayout, ReadbackSlotLease,
    ReadbackSlotPool, ReusableFrameBuffer, align_up, pack_rgba8_rows,
};
pub use schedule::{
    ExportSchedule, FrameRate, FrameRateError, FrameSample, SIMULATION_HZ, ScheduleError,
};
pub use sink::{
    BoundedFrameQueue, FfmpegConfig, FfmpegSink, FrameSink, PngFrameSink, SinkError, SinkReport,
};

/// Request a no-surface graphics context using the renderer's shared device
/// policy.  This async helper does not require an async runtime.
pub async fn request_headless_context() -> Result<GpuContext, GpuContextError> {
    GpuContext::request_headless().await
}

/// Synchronous convenience wrapper for native CLI callers.
pub fn request_headless_context_blocking() -> Result<GpuContext, GpuContextError> {
    pollster::block_on(request_headless_context())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_schedule_api_is_exact_and_end_exclusive() {
        let rate = FrameRate::new(30000, 1001).unwrap();
        let schedule = ExportSchedule::new(Rational::ZERO, Rational::ONE, rate).unwrap();
        assert_eq!(schedule.frame_count(), 30);
        assert_eq!(schedule.sample(0).unwrap().tick, 0);
        let nonzero = schedule.sample(1).unwrap();
        assert_eq!(nonzero.index, 1);
        assert_eq!(nonzero.time, Rational::new(1001, 30000).unwrap());
        assert_eq!(nonzero.tick, 4);
        assert!(matches!(
            schedule.sample(schedule.frame_count()),
            Err(ScheduleError::FrameOutOfRange)
        ));
    }
}
