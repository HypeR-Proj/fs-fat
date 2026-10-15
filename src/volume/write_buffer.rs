// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Operation-scoped sector coalescing. The owning Disk supplies complete sectors
//! and drains them before the volume operation returns. Dirty sectors also form
//! the authoritative read view until their device transfer completes.

use super::{BlockDevice, BlockError, DeviceSlot, Error, SECTOR_SIZE};
use crate::block::WriteRequest;
use alloc::{boxed::Box, vec::Vec};

const WINDOWS: usize = 8;
const WINDOW_BYTES: usize = 128 * 1024;
const WINDOW_SECTORS: usize = WINDOW_BYTES / SECTOR_SIZE;
const BITMAP_WORDS: usize = WINDOW_SECTORS / 64;
const BATCH_REQUESTS: usize = 4;

#[derive(Clone, Copy)]
struct Window {
    first: Option<u64>,
    dirty: [u64; BITMAP_WORDS],
}

impl Window {
    const EMPTY: Self = Self {
        first: None,
        dirty: [0; BITMAP_WORDS],
    };

    fn contains(&self, sector: u64) -> Option<usize> {
        let offset = sector.checked_sub(self.first?)?;
        let offset = usize::try_from(offset).ok()?;
        (offset < WINDOW_SECTORS && self.is_dirty(offset)).then_some(offset)
    }

    fn is_dirty(&self, offset: usize) -> bool {
        self.dirty[offset / 64] & (1 << (offset % 64)) != 0
    }
}

pub(super) struct Buffer {
    bytes: Vec<u8>,
    windows: [Window; WINDOWS],
    next: usize,
}

impl Buffer {
    pub(super) const fn allocation_bytes() -> usize {
        WINDOWS * WINDOW_BYTES + core::mem::size_of::<Self>()
    }

    pub(super) fn new() -> Result<Box<Self>, Error> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(WINDOWS * WINDOW_BYTES)
            .map_err(|_| Error::Allocation)?;
        bytes.resize(WINDOWS * WINDOW_BYTES, 0);
        crate::allocation::try_box(Self {
            bytes,
            windows: [Window::EMPTY; WINDOWS],
            next: 0,
        })
        .map_err(|_| Error::Allocation)
    }

    pub(super) fn sector(&self, sector: u64) -> Option<&[u8]> {
        for (index, window) in self.windows.iter().enumerate() {
            if let Some(offset) = window.contains(sector) {
                let start = index * WINDOW_BYTES + offset * SECTOR_SIZE;
                return Some(&self.bytes[start..start + SECTOR_SIZE]);
            }
        }
        None
    }

    pub(super) fn overlay(&self, first: u64, output: &mut [u8]) {
        // Every volume operation drains its writes before returning. Most bulk
        // reads therefore have no pending overlay; avoid probing eight empty
        // windows for each sector in a potentially 512 KiB transfer.
        if self.windows.iter().all(|window| window.first.is_none()) {
            return;
        }
        for (offset, sector) in output.chunks_exact_mut(SECTOR_SIZE).enumerate() {
            if let Some(pending) = self.sector(first + offset as u64) {
                sector.copy_from_slice(pending);
            }
        }
    }

    pub(super) fn write<D: BlockDevice>(
        &mut self,
        device: &DeviceSlot<D>,
        mut first: u64,
        mut input: &[u8],
    ) -> Result<(), BlockError> {
        super::super::block::validate_range(device.sectors, first, input.len())?;
        while !input.is_empty() {
            let base = first / WINDOW_SECTORS as u64 * WINDOW_SECTORS as u64;
            let index = match self
                .windows
                .iter()
                .position(|window| window.first == Some(base))
            {
                Some(index) => index,
                None => {
                    let index = self.next;
                    self.drain_windows(device, index..index + 1)?;
                    self.next = (index + 1) % WINDOWS;
                    self.windows[index].first = Some(base);
                    index
                }
            };
            let offset = (first - base) as usize;
            let count = (input.len() / SECTOR_SIZE).min(WINDOW_SECTORS - offset);
            let length = count * SECTOR_SIZE;
            let start = index * WINDOW_BYTES + offset * SECTOR_SIZE;
            self.bytes[start..start + length].copy_from_slice(&input[..length]);
            for sector in offset..offset + count {
                self.windows[index].dirty[sector / 64] |= 1 << (sector % 64);
            }
            first += count as u64;
            input = &input[length..];
        }
        Ok(())
    }

    pub(super) fn drain<D: BlockDevice>(
        &mut self,
        device: &DeviceSlot<D>,
    ) -> Result<(), BlockError> {
        self.drain_windows(device, 0..WINDOWS)?;
        self.next = 0;
        Ok(())
    }

    fn drain_windows<D: BlockDevice>(
        &mut self,
        device: &DeviceSlot<D>,
        windows: core::ops::Range<usize>,
    ) -> Result<(), BlockError> {
        let mut requests = [WriteRequest {
            first: 0,
            bytes: &[],
        }; BATCH_REQUESTS];
        let mut count = 0;
        for index in windows.clone() {
            let window = self.windows[index];
            let Some(first) = window.first else { continue };
            let mut sector = 0;
            while sector < WINDOW_SECTORS {
                if !window.is_dirty(sector) {
                    sector += 1;
                    continue;
                }
                let start = sector;
                while sector < WINDOW_SECTORS && window.is_dirty(sector) {
                    sector += 1;
                }
                let bytes = index * WINDOW_BYTES + start * SECTOR_SIZE;
                let length = (sector - start) * SECTOR_SIZE;
                requests[count] = WriteRequest {
                    first: first + start as u64,
                    bytes: &self.bytes[bytes..bytes + length],
                };
                count += 1;
                if count == BATCH_REQUESTS {
                    device.access(|d| d.write_batch(&requests))?;
                    count = 0;
                }
            }
        }
        if count != 0 {
            device.access(|d| d.write_batch(&requests[..count]))?;
        }
        // Clear only after the entire drain succeeds. A partially completed
        // batch poisons the device, so no uncertain input is reused or retried.
        for index in windows {
            self.windows[index] = Window::EMPTY;
        }
        Ok(())
    }
}
