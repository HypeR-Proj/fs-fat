// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded allocation maps and small-read windows. The volume owner serializes
//! reads and invalidates both before mutation, including partially failed writes.

use super::{BlockDevice, BlockError, DeviceSlot, Disk, Error, SECTOR_SIZE, sector_cache};
use alloc::{boxed::Box, string::String, vec::Vec};

const FILES: usize = 4;
const EXTENTS: usize = 128;
const PATH_BYTES: usize = 4096;

struct Extent {
    logical: u64,
    physical: u64,
    length: u64,
}

pub(super) struct Map {
    path: String,
    extents: Vec<Extent>,
    size: u64,
    complete: bool,
    valid: bool,
    cache: Box<sector_cache::Cache>,
}

pub(super) struct Cache {
    maps: Vec<Option<Map>>,
    next: usize,
}

impl Cache {
    pub(super) const fn allocation_bytes() -> usize {
        FILES
            * (core::mem::size_of::<Option<Map>>()
                + PATH_BYTES
                + EXTENTS * core::mem::size_of::<Extent>()
                + sector_cache::Cache::allocation_bytes())
    }

    pub(super) fn new() -> Result<Self, Error> {
        let mut maps = Vec::new();
        maps.try_reserve_exact(FILES)
            .map_err(|_| Error::Allocation)?;
        for _ in 0..FILES {
            let mut path = String::new();
            path.try_reserve_exact(PATH_BYTES)
                .map_err(|_| Error::Allocation)?;
            let mut extents = Vec::new();
            extents
                .try_reserve_exact(EXTENTS)
                .map_err(|_| Error::Allocation)?;
            maps.push(Some(Map {
                path,
                extents,
                size: 0,
                complete: false,
                valid: false,
                cache: sector_cache::Cache::new()?,
            }));
        }
        Ok(Self { maps, next: 0 })
    }

    pub(super) fn invalidate(&mut self) {
        for map in self.maps.iter_mut().flatten() {
            map.valid = false;
        }
    }

    pub(super) fn select(&mut self, path: &str) -> usize {
        if let Some(index) = self
            .maps
            .iter()
            .position(|map| map.as_ref().is_some_and(|m| m.matches(path)))
        {
            return index;
        }
        let index = self.next;
        self.next = (self.next + 1) % FILES;
        index
    }

    pub(super) fn take(&mut self, index: usize) -> Result<Map, Error> {
        self.maps
            .get_mut(index)
            .and_then(Option::take)
            .ok_or(Error::Corrupt)
    }

    pub(super) fn put(&mut self, index: usize, map: Map) {
        self.maps[index] = Some(map);
    }
}

impl Map {
    pub(super) fn matches(&self, path: &str) -> bool {
        self.valid && self.path == path
    }

    pub(super) const fn complete(&self) -> bool {
        self.complete
    }

    pub(super) fn prepare<D: BlockDevice>(
        &mut self,
        fs: &fatfs::FileSystem<Disk<D>, crate::fat_time::Clock>,
        path: &str,
        sectors: u64,
    ) -> Result<(), Error> {
        self.valid = false;
        self.complete = false;
        self.extents.clear();
        self.cache.invalidate();
        self.path.clear();
        self.path.push_str(path);
        let mut file = fs.root_dir().open_file(path).map_err(Error::from)?;
        self.size = u64::from(file.size().ok_or(Error::Corrupt)?);
        let mut logical = 0u64;
        for extent in file.extents() {
            if logical == self.size {
                break;
            }
            let extent = extent.map_err(Error::from)?;
            let length = u64::from(extent.size);
            if length == 0
                || extent.offset % SECTOR_SIZE as u64 != 0
                || extent
                    .offset
                    .checked_add(length)
                    .is_none_or(|end| end > sectors * SECTOR_SIZE as u64)
                || logical
                    .checked_add(length)
                    .is_none_or(|end| end > self.size)
            {
                return Err(Error::Corrupt);
            }
            if let Some(previous) = self.extents.last_mut()
                && previous.physical + previous.length == extent.offset
            {
                previous.length += length;
            } else {
                if self.extents.len() == EXTENTS {
                    // Remember the miss until mutation/eviction to avoid rescanning
                    // a fragmented chain just to rediscover the cache bound.
                    self.valid = true;
                    return Ok(());
                }
                self.extents.push(Extent {
                    logical,
                    physical: extent.offset,
                    length,
                });
            }
            logical += length;
        }
        if logical != self.size {
            return Err(Error::Corrupt);
        }
        self.complete = true;
        self.valid = true;
        Ok(())
    }

    pub(super) fn read<D: BlockDevice>(
        &mut self,
        device: &DeviceSlot<D>,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, BlockError> {
        let count = output.len().min(self.size.saturating_sub(offset) as usize);
        let mut done = 0;
        let first = self
            .extents
            .partition_point(|extent| extent.logical + extent.length <= offset);
        for extent in &self.extents[first..] {
            if done == count {
                break;
            }
            let relative = offset + done as u64 - extent.logical;
            let mut physical = extent.physical + relative;
            let end = done + (count - done).min((extent.length - relative) as usize);
            while done < end {
                let within = physical as usize % SECTOR_SIZE;
                let n = if within == 0 && end - done >= SECTOR_SIZE {
                    let length = (end - done) / SECTOR_SIZE * SECTOR_SIZE;
                    device.access(|d| {
                        d.read_sectors(
                            physical / SECTOR_SIZE as u64,
                            &mut output[done..done + length],
                        )
                    })?;
                    length
                } else {
                    let sector = self.cache.sector(device, physical / SECTOR_SIZE as u64)?;
                    let length = (end - done).min(SECTOR_SIZE - within);
                    output[done..done + length].copy_from_slice(&sector[within..within + length]);
                    length
                };
                done += n;
                physical += n as u64;
            }
        }
        Ok(done)
    }
}
