// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Small read-through windows keep directory and FAT lookups from evicting
//! each other. The owning Disk overlays pending writes before publishing a
//! refill and updates resident windows when staging new sector writes. Failed
//! writes poison the volume before any subsequent cache access.

use super::{BlockDevice, BlockError, DeviceSlot, Error, SECTOR_SIZE};
use alloc::{boxed::Box, vec::Vec};

const WINDOWS: usize = 4;
const WINDOW_BYTES: usize = 4096;
const WINDOW_SECTORS: u64 = (WINDOW_BYTES / SECTOR_SIZE) as u64;

pub(super) struct Cache {
    bytes: Vec<u8>,
    first: [Option<u64>; WINDOWS],
    next: usize,
}

impl Cache {
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
            first: [None; WINDOWS],
            next: 0,
        })
        .map_err(|_| Error::Allocation)
    }

    pub(super) fn invalidate(&mut self) {
        self.first.fill(None);
    }

    pub(super) fn written(&mut self, first: u64, input: &[u8]) {
        let start = first * SECTOR_SIZE as u64;
        let end = start + input.len() as u64;
        for (index, window) in self.first.iter().enumerate() {
            let Some(window) = window else { continue };
            let window = window * SECTOR_SIZE as u64;
            let low = start.max(window);
            let high = end.min(window + WINDOW_BYTES as u64);
            if low < high {
                let output = index * WINDOW_BYTES + (low - window) as usize;
                let input_offset = (low - start) as usize;
                let length = (high - low) as usize;
                self.bytes[output..output + length]
                    .copy_from_slice(&input[input_offset..input_offset + length]);
            }
        }
    }

    pub(super) fn sector<D: BlockDevice>(
        &mut self,
        device: &DeviceSlot<D>,
        sector: u64,
    ) -> Result<&[u8], BlockError> {
        self.sector_with_overlay(device, sector, |_, _| {})
    }

    /// Merge the caller's pending sectors into a fresh window before it becomes
    /// readable. Overlay the whole window: a neighboring sector may remain in
    /// this cache after its pending write has drained or been evicted.
    pub(super) fn sector_with_overlay<D: BlockDevice>(
        &mut self,
        device: &DeviceSlot<D>,
        sector: u64,
        overlay: impl FnOnce(u64, &mut [u8]),
    ) -> Result<&[u8], BlockError> {
        let first = sector / WINDOW_SECTORS * WINDOW_SECTORS;
        let index = if let Some(index) = self.first.iter().position(|entry| *entry == Some(first)) {
            index
        } else {
            let index = self.next;
            self.next = (index + 1) % WINDOWS;
            self.first[index] = None;
            // A final partial window must never issue I/O beyond the volume.
            let count = (device.sectors - first).min(WINDOW_SECTORS) as usize * SECTOR_SIZE;
            let start = index * WINDOW_BYTES;
            device.access(|d| d.read_sectors(first, &mut self.bytes[start..start + count]))?;
            overlay(first, &mut self.bytes[start..start + count]);
            self.first[index] = Some(first);
            index
        };
        let start = index * WINDOW_BYTES + (sector - first) as usize * SECTOR_SIZE;
        Ok(&self.bytes[start..start + SECTOR_SIZE])
    }
}
