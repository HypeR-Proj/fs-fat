// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Operation-scoped sector coalescing. The owning Disk supplies complete sectors
//! and joins submitted writes before the volume operation returns. Dirty sectors
//! form the authoritative read view before submission; the device queue orders
//! later overlapping accesses after copied submissions. Readonly request-window
//! payloads bypass these buffers and retain kernel-owned source references.

use super::{BlockDevice, BlockError, DeviceSlot, Error, SECTOR_SIZE};
use crate::block::{WRITE_BATCH_BYTES, WRITE_BATCH_REQUESTS, WriteRequest};
use alloc::{boxed::Box, vec::Vec};

const WINDOWS: usize = 8;
const WINDOW_BYTES: usize = 128 * 1024;
const WINDOW_SECTORS: usize = WINDOW_BYTES / SECTOR_SIZE;
const BITMAP_WORDS: usize = WINDOW_SECTORS / 64;
const SUBMIT_SECTORS: usize = 64 * 1024 / SECTOR_SIZE;

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

    fn overlaps_dirty(&self, first: u64, end: u64) -> bool {
        let Some(base) = self.first else { return false };
        let low = first.saturating_sub(base).min(WINDOW_SECTORS as u64) as usize;
        let high = end.saturating_sub(base).min(WINDOW_SECTORS as u64) as usize;
        (low..high).any(|sector| self.is_dirty(sector))
    }

    fn ready(&self) -> bool {
        // A 128KiB file write may straddle two physical windows without ever
        // filling either. Submit a contiguous 64KiB run early instead. Only
        // runs spanning bitmap words can reach this threshold, so checking
        // each word's low/high runs needs no sector-by-sector scan.
        let mut run = 0;
        for word in self.dirty {
            let low = word.trailing_ones() as usize;
            run += low;
            if run >= SUBMIT_SECTORS {
                return true;
            }
            if low != 64 {
                run = word.leading_ones() as usize;
            }
        }
        false
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
        if !input.is_empty() && device.access(|d| Ok(d.is_write_window(input)))? {
            // Only overlapping buffered writes must precede this source range.
            // Draining unrelated FAT sectors here turns every cluster allocation
            // into separate metadata I/O and also breaks source-range merging.
            let end = first + (input.len() / SECTOR_SIZE) as u64;
            for index in 0..WINDOWS {
                if self.windows[index].overlaps_dirty(first, end) {
                    self.drain_windows(device, index..index + 1)?;
                }
            }
            return device.access(|d| d.submit_write_window(first, input));
        }
        while !input.is_empty() {
            let base = first / WINDOW_SECTORS as u64 * WINDOW_SECTORS as u64;
            let index = match self
                .windows
                .iter()
                .position(|window| window.first == Some(base))
            {
                Some(index) => index,
                None => {
                    // Early submitted data windows are immediately reusable.
                    // Keep sparse metadata coalesced instead of evicting it
                    // merely because the round-robin cursor reached it first.
                    let index = (0..WINDOWS)
                        .map(|offset| (self.next + offset) % WINDOWS)
                        .find(|index| self.windows[*index].first.is_none())
                        .unwrap_or(self.next);
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
            // Copy-on-submit lets the device run while FAT walks the remaining
            // cluster chain, including when the file starts between physical
            // window boundaries. Sparse metadata windows remain buffered.
            if self.windows[index].ready() {
                self.drain_windows(device, index..index + 1)?;
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
        }; WRITE_BATCH_REQUESTS];
        let mut count = 0;
        let mut payload = 0;
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
                if count != 0 && payload + length > WRITE_BATCH_BYTES {
                    device.access(|d| d.submit_writes(&requests[..count]))?;
                    count = 0;
                    payload = 0;
                }
                requests[count] = WriteRequest {
                    first: first + start as u64,
                    bytes: &self.bytes[bytes..bytes + length],
                };
                count += 1;
                payload += length;
                if count == WRITE_BATCH_REQUESTS {
                    device.access(|d| d.submit_writes(&requests))?;
                    count = 0;
                    payload = 0;
                }
            }
        }
        if count != 0 {
            device.access(|d| d.submit_writes(&requests[..count]))?;
        }
        // Every accepted range has an independent copy. Later overlapping
        // requests are ordered by the device queue; operation exit joins all
        // writes. A failed drain poisons the volume instead of retrying input.
        for index in windows {
            self.windows[index] = Window::EMPTY;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::*;
    use alloc::vec;
    use core::cell::RefCell;
    use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64};

    struct Recorder(Vec<(u64, usize, u8)>, Option<(usize, usize)>);

    impl BlockDevice for Recorder {
        fn sector_count(&self) -> u64 {
            16 * WINDOW_SECTORS as u64
        }
        fn read_sectors(&mut self, _: u64, _: &mut [u8]) -> Result<(), BlockError> {
            Err(BlockError::Io)
        }
        fn write_sectors(&mut self, first: u64, bytes: &[u8]) -> Result<(), BlockError> {
            self.0.push((first, bytes.len(), bytes[0]));
            Ok(())
        }
        fn is_write_window(&self, input: &[u8]) -> bool {
            self.1.is_some_and(|(base, length)| {
                (input.as_ptr() as usize)
                    .checked_sub(base)
                    .and_then(|offset| offset.checked_add(input.len()))
                    .is_some_and(|end| end <= length)
            })
        }
        fn submit_write_window(&mut self, first: u64, input: &[u8]) -> Result<(), BlockError> {
            self.write_sectors(first, input)
        }
        fn flush(&mut self) -> Result<(), BlockError> {
            Ok(())
        }
    }

    fn checked<T, E: core::fmt::Debug>(result: Result<T, E>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("unexpected error: {error:?}"),
        }
    }

    #[test]
    fn empty_submitted_windows_preserve_sparse_metadata_until_drain() {
        let device = DeviceSlot {
            device: RefCell::new(Some(Recorder(Vec::new(), None))),
            enabled: AtomicBool::new(true),
            remaining: AtomicU64::new(1000),
            failed: AtomicU8::new(0),
            sectors: 16 * WINDOW_SECTORS as u64,
            readonly: false,
        };
        let mut buffer = checked(Buffer::new());
        checked(buffer.write(&device, 0, &[0x11; SECTOR_SIZE]));
        let data = vec![0x5a; SUBMIT_SECTORS * SECTOR_SIZE];
        for window in 1..=WINDOWS + 1 {
            checked(buffer.write(&device, (window * WINDOW_SECTORS) as u64, &data));
        }
        checked(device.access(|recorder| {
            assert!(recorder.0.iter().all(|write| write.0 != 0));
            assert_eq!(recorder.0.len(), WINDOWS + 1);
            Ok(())
        }));
        checked(buffer.write(&device, 0, &[0x22; SECTOR_SIZE]));
        checked(buffer.drain(&device));
        checked(device.access(|recorder| {
            let metadata: Vec<_> = recorder.0.iter().filter(|write| write.0 == 0).collect();
            assert_eq!(metadata, [&(0, SECTOR_SIZE, 0x22)]);
            Ok(())
        }));
    }

    #[test]
    fn source_window_orders_overlaps_but_preserves_adjacent_dirty_sectors() {
        let source = [0x44; SECTOR_SIZE];
        let device = DeviceSlot {
            device: RefCell::new(Some(Recorder(
                Vec::new(),
                Some((source.as_ptr() as usize, source.len())),
            ))),
            enabled: AtomicBool::new(true),
            remaining: AtomicU64::new(1000),
            failed: AtomicU8::new(0),
            sectors: 16 * WINDOW_SECTORS as u64,
            readonly: false,
        };
        let mut buffer = checked(Buffer::new());
        checked(buffer.write(&device, 3, &[0x11; SECTOR_SIZE]));
        checked(buffer.write(&device, 5, &[0x22; SECTOR_SIZE]));
        checked(buffer.write(&device, 4, &source));
        checked(device.access(|recorder| {
            assert_eq!(recorder.0, [(4, SECTOR_SIZE, 0x44)]);
            Ok(())
        }));
        assert_eq!(buffer.sector(3), Some(&[0x11; SECTOR_SIZE][..]));
        assert_eq!(buffer.sector(5), Some(&[0x22; SECTOR_SIZE][..]));

        checked(buffer.write(&device, 3, &source));
        assert!(buffer.sector(3).is_none());
        checked(buffer.write(&device, 4, &[0x55; SECTOR_SIZE]));
        checked(buffer.write(&device, 4, &source));
        checked(buffer.drain(&device));
        checked(device.access(|recorder| {
            assert_eq!(
                recorder.0,
                [
                    (4, SECTOR_SIZE, 0x44),
                    (3, SECTOR_SIZE, 0x11),
                    (5, SECTOR_SIZE, 0x22),
                    (3, SECTOR_SIZE, 0x44),
                    (4, SECTOR_SIZE, 0x55),
                    (4, SECTOR_SIZE, 0x44),
                ]
            );
            Ok(())
        }));
    }
}
