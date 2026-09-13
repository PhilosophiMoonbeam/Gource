// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Checked texture readback layout and reusable CPU/GPU buffers.

use std::sync::mpsc;
use std::time::Duration;

/// WebGPU requires every texture-copy row stride to be aligned to 256 bytes.
pub const COPY_BYTES_PER_ROW_ALIGNMENT: usize = 256;

/// Checked RGBA8 texture-copy dimensions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadbackLayout {
    pub width: u32,
    pub height: u32,
    pub unpadded_bytes_per_row: usize,
    pub padded_bytes_per_row: usize,
    pub byte_len: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ReadbackError {
    #[error("readback extent must be non-zero")]
    ZeroExtent,
    #[error("readback dimensions overflow")]
    Overflow,
    #[error("readback slot count must be positive")]
    ZeroSlots,
    #[error("readback byte cap exceeded: required {required}, cap {cap}")]
    ByteCap { required: u64, cap: u64 },
    #[error("readback slot id {0} is invalid")]
    InvalidSlot(usize),
    #[error("readback slot {0} is not in use")]
    SlotNotInUse(usize),
    #[error("GPU readback mapping failed")]
    Mapping,
    #[error("GPU readback poll failed: {0}")]
    Poll(String),
    #[error("GPU readback callback timed out")]
    Timeout,
    #[error("GPU readback source is too short: expected {expected}, got {actual}")]
    SourceTooShort { expected: usize, actual: usize },
    #[error("GPU readback destination is too short: expected {expected}, got {actual}")]
    DestinationTooShort { expected: usize, actual: usize },
}

impl ReadbackLayout {
    /// Compute the checked RGBA8 copy footprint.
    pub fn rgba8(width: u32, height: u32) -> Result<Self, ReadbackError> {
        if width == 0 || height == 0 {
            return Err(ReadbackError::ZeroExtent);
        }
        let width = width as usize;
        let height = height as usize;
        let unpadded = width.checked_mul(4).ok_or(ReadbackError::Overflow)?;
        let padded = align_up(unpadded, COPY_BYTES_PER_ROW_ALIGNMENT)?;
        let byte_len = padded.checked_mul(height).ok_or(ReadbackError::Overflow)?;
        Ok(Self {
            width: width as u32,
            height: height as u32,
            unpadded_bytes_per_row: unpadded,
            padded_bytes_per_row: padded,
            byte_len,
        })
    }

    #[must_use]
    pub const fn row_count(self) -> usize {
        self.height as usize
    }
}

pub fn align_up(value: usize, alignment: usize) -> Result<usize, ReadbackError> {
    if alignment == 0 {
        return Err(ReadbackError::Overflow);
    }
    let remainder = value % alignment;
    if remainder == 0 {
        Ok(value)
    } else {
        value
            .checked_add(alignment - remainder)
            .ok_or(ReadbackError::Overflow)
    }
}

/// Strip 256-byte row padding into a tightly packed RGBA8 frame.
pub fn pack_rgba8_rows(
    layout: ReadbackLayout,
    source: &[u8],
    destination: &mut [u8],
) -> Result<(), ReadbackError> {
    if source.len() < layout.byte_len {
        return Err(ReadbackError::SourceTooShort {
            expected: layout.byte_len,
            actual: source.len(),
        });
    }
    let output_len = layout
        .unpadded_bytes_per_row
        .checked_mul(layout.height as usize)
        .ok_or(ReadbackError::Overflow)?;
    if destination.len() < output_len {
        return Err(ReadbackError::DestinationTooShort {
            expected: output_len,
            actual: destination.len(),
        });
    }
    for row in 0..layout.height as usize {
        let source_start = row * layout.padded_bytes_per_row;
        let destination_start = row * layout.unpadded_bytes_per_row;
        destination[destination_start..destination_start + layout.unpadded_bytes_per_row]
            .copy_from_slice(&source[source_start..source_start + layout.unpadded_bytes_per_row]);
    }
    Ok(())
}

/// A reusable tight RGBA8 CPU frame buffer.
#[derive(Clone, Debug, Default)]
pub struct ReusableFrameBuffer {
    bytes: Vec<u8>,
}

impl ReusableFrameBuffer {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
        }
    }

    pub fn resize_for(&mut self, layout: ReadbackLayout) -> Result<(), ReadbackError> {
        let len = layout
            .unpadded_bytes_per_row
            .checked_mul(layout.height as usize)
            .ok_or(ReadbackError::Overflow)?;
        self.bytes.resize(len, 0);
        Ok(())
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotState {
    Free,
    InUse,
}

/// A bounded slot lifecycle independent of GPU handles.
///
/// `try_acquire` never allocates or silently evicts a slot.  A caller must
/// release each acquired slot after the associated frame has been consumed.
#[derive(Clone, Debug)]
pub struct ReadbackSlotPool {
    layout: ReadbackLayout,
    states: Vec<SlotState>,
    max_bytes: u64,
}

impl ReadbackSlotPool {
    pub fn new(
        layout: ReadbackLayout,
        slot_count: usize,
        max_bytes: u64,
    ) -> Result<Self, ReadbackError> {
        if slot_count == 0 {
            return Err(ReadbackError::ZeroSlots);
        }
        let required = (layout.byte_len as u64)
            .checked_mul(slot_count as u64)
            .ok_or(ReadbackError::Overflow)?;
        if required > max_bytes {
            return Err(ReadbackError::ByteCap {
                required,
                cap: max_bytes,
            });
        }
        Ok(Self {
            layout,
            states: vec![SlotState::Free; slot_count],
            max_bytes,
        })
    }

    #[must_use]
    pub fn layout(&self) -> ReadbackLayout {
        self.layout
    }

    #[must_use]
    pub fn slot_count(&self) -> usize {
        self.states.len()
    }

    #[must_use]
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    #[must_use]
    pub fn in_use(&self) -> usize {
        self.states
            .iter()
            .filter(|state| **state == SlotState::InUse)
            .count()
    }

    pub fn try_acquire(&mut self) -> Option<ReadbackSlotLease> {
        let index = self
            .states
            .iter()
            .position(|state| *state == SlotState::Free)?;
        self.states[index] = SlotState::InUse;
        Some(ReadbackSlotLease { index })
    }

    pub fn release(&mut self, slot: ReadbackSlotLease) -> Result<(), ReadbackError> {
        self.release_id(slot.index)
    }

    pub fn release_id(&mut self, index: usize) -> Result<(), ReadbackError> {
        let state = self
            .states
            .get_mut(index)
            .ok_or(ReadbackError::InvalidSlot(index))?;
        if *state != SlotState::InUse {
            return Err(ReadbackError::SlotNotInUse(index));
        }
        *state = SlotState::Free;
        Ok(())
    }
}

/// An acquired bounded readback slot.  It is intentionally not `Copy` so a
/// caller cannot accidentally release the same slot twice.
#[derive(Debug)]
pub struct ReadbackSlotLease {
    index: usize,
}

impl ReadbackSlotLease {
    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }
}

/// A reusable mapped GPU readback buffer for one RGBA8 target.
pub struct GpuReadback {
    pub layout: ReadbackLayout,
    pub buffer: wgpu::Buffer,
    cpu: ReusableFrameBuffer,
}

impl GpuReadback {
    pub fn new(
        device: &wgpu::Device,
        layout: ReadbackLayout,
        label: Option<&str>,
    ) -> Result<Self, ReadbackError> {
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label,
            size: layout.byte_len as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut cpu = ReusableFrameBuffer::with_capacity(
            layout
                .unpadded_bytes_per_row
                .checked_mul(layout.height as usize)
                .ok_or(ReadbackError::Overflow)?,
        );
        cpu.resize_for(layout)?;
        Ok(Self {
            layout,
            buffer,
            cpu,
        })
    }

    /// Record a texture-to-buffer copy into the caller's command encoder.
    pub fn encode_copy(&self, encoder: &mut wgpu::CommandEncoder, texture: &wgpu::Texture) {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.layout.padded_bytes_per_row as u32),
                    rows_per_image: Some(self.layout.height),
                },
            },
            wgpu::Extent3d {
                width: self.layout.width,
                height: self.layout.height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Wait for the copy, map once, and copy into the reusable tight buffer.
    pub fn read_rgba8(
        &mut self,
        device: &wgpu::Device,
        timeout: Option<Duration>,
    ) -> Result<&[u8], ReadbackError> {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result.map_err(|_| ReadbackError::Mapping));
            });
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout,
            })
            .map_err(|error| ReadbackError::Poll(error.to_string()))?;
        receiver.recv().map_err(|_| ReadbackError::Mapping)??;
        {
            let mapped = self
                .buffer
                .slice(..)
                .get_mapped_range()
                .map_err(|_| ReadbackError::Mapping)?;
            pack_rgba8_rows(self.layout, &mapped, self.cpu.as_mut_slice())?;
        }
        self.buffer.unmap();
        Ok(self.cpu.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_width_uses_256_byte_rows_and_strips_padding() {
        let layout = ReadbackLayout::rgba8(3, 2).unwrap();
        assert_eq!(layout.unpadded_bytes_per_row, 12);
        assert_eq!(layout.padded_bytes_per_row, 256);
        let mut source = vec![0u8; layout.byte_len];
        source[..12].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        source[256..268].copy_from_slice(&[13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24]);
        let mut destination = vec![0u8; 24];
        pack_rgba8_rows(layout, &source, &mut destination).unwrap();
        assert_eq!(destination, (1u8..=24).collect::<Vec<_>>());
    }

    #[test]
    fn slots_apply_count_and_byte_caps_without_eviction() {
        let layout = ReadbackLayout::rgba8(1, 1).unwrap();
        assert!(ReadbackSlotPool::new(layout, 2, 511).is_err());
        let mut pool = ReadbackSlotPool::new(layout, 2, 512).unwrap();
        let first = pool.try_acquire().unwrap();
        let second = pool.try_acquire().unwrap();
        assert!(pool.try_acquire().is_none());
        pool.release(first).unwrap();
        assert_eq!(pool.try_acquire().unwrap().index(), 0);
        pool.release(second).unwrap();
    }
}
