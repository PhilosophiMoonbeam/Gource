// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Exact frame-rate arithmetic for export.  The schedule is independent of
//! wall-clock cadence and samples the canonical 120 Hz replay with floor
//! semantics.

use std::fmt;

use gource_core::{Rational, RationalError};
use serde::{Deserialize, Serialize};

/// The simulation frequency used by the replay contract.
pub const SIMULATION_HZ: u64 = 120;

/// A positive rational output frame rate.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct FrameRate {
    /// Frames per second numerator.
    pub numerator: u64,
    /// Frames per second denominator.
    pub denominator: u64,
}

/// Invalid frame-rate input.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum FrameRateError {
    #[error("frame-rate numerator must be positive")]
    ZeroNumerator,
    #[error("frame-rate denominator must be positive")]
    ZeroDenominator,
    #[error("frame-rate arithmetic overflow")]
    Overflow,
}

impl FrameRate {
    /// Construct and reduce a positive rational rate.
    pub fn new(numerator: u64, denominator: u64) -> Result<Self, FrameRateError> {
        if numerator == 0 {
            return Err(FrameRateError::ZeroNumerator);
        }
        if denominator == 0 {
            return Err(FrameRateError::ZeroDenominator);
        }
        let divisor = gcd(numerator, denominator);
        Ok(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }

    /// Construct an integral frame rate.
    pub fn integer(fps: u64) -> Result<Self, FrameRateError> {
        Self::new(fps, 1)
    }

    /// Return the rate as a floating point value for diagnostics only.
    #[must_use]
    pub fn as_f64(self) -> f64 {
        self.numerator as f64 / self.denominator as f64
    }

    /// Return the exact duration of one output frame.
    pub fn frame_duration(self) -> Result<Rational, RationalError> {
        Rational::new(self.denominator as i128, self.numerator as i128)
    }

    /// Format this rate for native process arguments without float conversion.
    #[must_use]
    pub fn argv_value(self) -> String {
        format!("{}/{}", self.numerator, self.denominator)
    }
}

impl Default for FrameRate {
    fn default() -> Self {
        Self {
            numerator: 60,
            denominator: 1,
        }
    }
}

/// One exact frame sample in an export schedule.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FrameSample {
    /// Zero-based output frame index.
    pub index: u64,
    /// Requested output time in wall/export seconds.
    pub time: Rational,
    /// Canonical replay tick selected with floor semantics.
    pub tick: u64,
}

/// A finite, end-exclusive export schedule.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExportSchedule {
    pub start: Rational,
    pub end: Rational,
    pub frame_rate: FrameRate,
    frame_count: u64,
}

/// Schedule construction or sampling failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ScheduleError {
    #[error("export start time must be non-negative")]
    NegativeStart,
    #[error("export end time must not precede start time")]
    ReversedRange,
    #[error("export schedule rational arithmetic overflow")]
    Overflow,
    #[error("frame index is outside the end-exclusive export range")]
    FrameOutOfRange,
    #[error("canonical tick does not fit in u64")]
    TickOverflow,
}

impl ExportSchedule {
    /// Build an end-exclusive schedule.  The frame count is exactly
    /// `ceil((end - start) * fps)`; an exact endpoint never emits a frame.
    pub fn new(
        start: Rational,
        end: Rational,
        frame_rate: FrameRate,
    ) -> Result<Self, ScheduleError> {
        if start.numerator < 0 {
            return Err(ScheduleError::NegativeStart);
        }
        if end < start {
            return Err(ScheduleError::ReversedRange);
        }
        let delta_num = end
            .numerator
            .checked_mul(start.denominator)
            .and_then(|value| {
                start
                    .numerator
                    .checked_mul(end.denominator)
                    .and_then(|start_num| value.checked_sub(start_num))
            })
            .ok_or(ScheduleError::Overflow)?;
        let delta_den = end
            .denominator
            .checked_mul(start.denominator)
            .ok_or(ScheduleError::Overflow)?;
        let scaled_num = delta_num
            .checked_mul(frame_rate.numerator as i128)
            .ok_or(ScheduleError::Overflow)?;
        let scaled_den = delta_den
            .checked_mul(frame_rate.denominator as i128)
            .ok_or(ScheduleError::Overflow)?;
        let frame_count = if scaled_num == 0 {
            0
        } else {
            let quotient = scaled_num / scaled_den;
            let remainder = scaled_num % scaled_den;
            quotient
                .checked_add(if remainder != 0 { 1 } else { 0 })
                .ok_or(ScheduleError::Overflow)?
                .try_into()
                .map_err(|_| ScheduleError::Overflow)?
        };
        Ok(Self {
            start,
            end,
            frame_rate,
            frame_count,
        })
    }

    #[must_use]
    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frame_count == 0
    }

    /// Compute one sample using exact rational arithmetic.
    pub fn sample(&self, index: u64) -> Result<FrameSample, ScheduleError> {
        if index >= self.frame_count {
            return Err(ScheduleError::FrameOutOfRange);
        }
        let offset_num = (index as i128)
            .checked_mul(self.frame_rate.denominator as i128)
            .ok_or(ScheduleError::Overflow)?;
        let offset_den = self.frame_rate.numerator as i128;
        let offset = Rational::new(offset_num, offset_den).map_err(|_| ScheduleError::Overflow)?;
        let time = self
            .start
            .checked_add(offset)
            .map_err(|_| ScheduleError::Overflow)?;
        let tick_num = time
            .numerator
            .checked_mul(SIMULATION_HZ as i128)
            .ok_or(ScheduleError::Overflow)?;
        let tick = tick_num
            .div_euclid(time.denominator)
            .try_into()
            .map_err(|_| ScheduleError::TickOverflow)?;
        Ok(FrameSample { index, time, tick })
    }

    /// Iterate all samples without allocating a frame list.
    pub fn samples(&self) -> impl Iterator<Item = Result<FrameSample, ScheduleError>> + '_ {
        (0..self.frame_count).map(|index| self.sample(index))
    }
}

impl fmt::Display for FrameRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.denominator == 1 {
            write!(f, "{}", self.numerator)
        } else {
            write!(f, "{}/{}", self.numerator, self.denominator)
        }
    }
}

fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rational(numerator: i128, denominator: i128) -> Rational {
        Rational::new(numerator, denominator).unwrap()
    }

    #[test]
    fn rational_rate_reduces_and_counts_end_exclusive() {
        let rate = FrameRate::new(600, 20).unwrap();
        assert_eq!(rate, FrameRate::integer(30).unwrap());
        let schedule = ExportSchedule::new(rational(0, 1), rational(1, 1), rate).unwrap();
        assert_eq!(schedule.frame_count(), 30);
        assert_eq!(schedule.sample(29).unwrap().tick, 116);
        assert!(schedule.sample(30).is_err());
    }

    #[test]
    fn fractional_count_uses_ceil_without_endpoint_frame() {
        let rate = FrameRate::new(2, 1).unwrap();
        let schedule = ExportSchedule::new(rational(1, 3), rational(5, 6), rate).unwrap();
        assert_eq!(schedule.frame_count(), 1);
        assert_eq!(schedule.sample(0).unwrap().time, rational(1, 3));
        let exact = ExportSchedule::new(rational(0, 1), rational(1, 1), rate).unwrap();
        assert_eq!(exact.frame_count(), 2);
    }

    #[test]
    fn floor_sampling_handles_non_integral_120_hz_tick() {
        let rate = FrameRate::integer(24).unwrap();
        let schedule = ExportSchedule::new(rational(0, 1), rational(1, 24), rate).unwrap();
        assert_eq!(schedule.sample(0).unwrap().tick, 0);
        // 1/24 is five 120-Hz ticks exactly, but the endpoint is excluded.
        assert_eq!(schedule.frame_count(), 1);
    }
}
