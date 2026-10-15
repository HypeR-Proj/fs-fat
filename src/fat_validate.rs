// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Mount-time bounds validation before passing media to the upstream FAT code.
//! Exclusive volume ownership is required: a second writer invalidates this
//! proof as well as FAT's ordinary cache coherency contract.

use super::block::{BlockDevice, SECTOR_SIZE};
use super::volume::Error;
use alloc::vec::Vec;

pub(super) fn validate<D: BlockDevice>(
    disk: &mut D,
    boot: &[u8; SECTOR_SIZE],
) -> Result<(), Error> {
    let word = |p| u32::from_le_bytes([boot[p], boot[p + 1], boot[p + 2], boot[p + 3]]) as u64;
    let reserved = u16::from_le_bytes([boot[14], boot[15]]) as u64;
    let fat_sectors = word(36);
    let fats = boot[16] as u64;
    let data = reserved + fat_sectors * fats;
    let cluster_sectors = boot[13] as u64;
    let max_cluster = (word(32) - data) / cluster_sectors + 1;
    let root = word(44);
    let flags = u16::from_le_bytes([boot[40], boot[41]]);
    let active = if flags & 0x80 == 0 {
        0
    } else {
        (flags & 0xf) as u64
    };
    if active >= fats {
        return Err(Error::Corrupt);
    }
    let fat_start = reserved + active * fat_sectors;
    let mut sector = [0; SECTOR_SIZE];
    // Validate every possible successor before library multiplication and
    // subtraction. Reserved and end markers are handled by upstream itself.
    let mut scan = Vec::new();
    scan.try_reserve_exact(65536)
        .map_err(|_| Error::Allocation)?;
    scan.resize(65536, 0);
    for fat in 0..fats {
        let mut offset = 0;
        while offset < fat_sectors {
            let count = (fat_sectors - offset).min(128);
            let batch = &mut scan[..count as usize * SECTOR_SIZE];
            disk.read_sectors(reserved + fat * fat_sectors + offset, batch)
                .map_err(Error::Block)?;
            for (index, bytes) in batch.chunks_exact(4).enumerate() {
                let cluster = offset * 128 + index as u64;
                if cluster < 2 || cluster > max_cluster {
                    continue;
                }
                let next = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64
                    & 0x0fff_ffff;
                if next == 1 || (next > max_cluster && next < 0x0fff_fff7) {
                    return Err(Error::Corrupt);
                }
            }
            offset += count;
        }
    }
    drop(scan);
    let mut directories = Vec::new();
    directories.try_reserve(1).map_err(|_| Error::Allocation)?;
    directories.push(root);
    let bitmap_len = (max_cluster as usize + 4) / 4;
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(bitmap_len)
        .map_err(|_| Error::Allocation)?;
    owned.resize(bitmap_len, 0u8);
    mark(&mut owned, root, true)?;
    let mut fat_sector = [0; SECTOR_SIZE];
    let mut cached_fat_sector = None;
    let mut cursor = 0;
    while cursor < directories.len() {
        let mut cluster = directories[cursor];
        cursor += 1;
        let mut traversed = 0;
        let mut ended = false;
        loop {
            mark(&mut owned, cluster, false)?;
            traversed += 1;
            if traversed > max_cluster {
                return Err(Error::Corrupt);
            }
            for index in 0..cluster_sectors {
                if ended {
                    break;
                }
                disk.read_sectors(data + (cluster - 2) * cluster_sectors + index, &mut sector)
                    .map_err(Error::Block)?;
                for entry in sector.chunks_exact(32) {
                    if entry[0] == 0 {
                        ended = true;
                        break;
                    }
                    if entry[0] == 0xe5 || entry[11] == 0xf || entry[11] & 8 != 0 {
                        continue;
                    }
                    let first = ((u16::from_le_bytes([entry[20], entry[21]]) as u64) << 16)
                        | u16::from_le_bytes([entry[26], entry[27]]) as u64;
                    let size = u32::from_le_bytes([entry[28], entry[29], entry[30], entry[31]]);
                    if first == 1 || first > max_cluster || (first == 0 && size != 0) {
                        return Err(Error::Corrupt);
                    }
                    if entry[11] & 0x10 != 0
                        && &entry[..11] != b".          "
                        && &entry[..11] != b"..         "
                    {
                        if first < 2 {
                            return Err(Error::Corrupt);
                        }
                        mark(&mut owned, first, true)?;
                        // Explicit mount resource bound; never allocate from an
                        // attacker-controlled unbounded directory graph.
                        if directories.len() == 65536 {
                            return Err(Error::Unsupported);
                        }
                        directories.try_reserve(1).map_err(|_| Error::Allocation)?;
                        directories.push(first);
                    } else if entry[11] & 0x10 == 0 && first != 0 {
                        let mut item = first;
                        let mut allocated = 0u64;
                        loop {
                            mark(&mut owned, item, false)?;
                            allocated += cluster_sectors * 512;
                            let next = successor(
                                disk,
                                fat_start,
                                item,
                                &mut fat_sector,
                                &mut cached_fat_sector,
                            )?;
                            if next >= 0x0fff_fff8 {
                                break;
                            }
                            if !(2..=max_cluster).contains(&next) {
                                return Err(Error::Corrupt);
                            }
                            item = next;
                        }
                        if allocated < size as u64 {
                            return Err(Error::Corrupt);
                        }
                    }
                }
            }
            let next = successor(
                disk,
                fat_start,
                cluster,
                &mut fat_sector,
                &mut cached_fat_sector,
            )?;
            if next >= 0x0fff_fff8 {
                break;
            }
            if !(2..=max_cluster).contains(&next) {
                return Err(Error::Corrupt);
            }
            cluster = next;
        }
    }
    Ok(())
}

fn mark(bitmap: &mut [u8], cluster: u64, queued: bool) -> Result<(), Error> {
    let byte = bitmap.get_mut(cluster as usize / 4).ok_or(Error::Corrupt)?;
    let mask = 1 << ((cluster % 4) * 2 + u64::from(queued));
    if *byte & mask != 0 {
        return Err(Error::Corrupt);
    }
    *byte |= mask;
    Ok(())
}

fn successor<D: BlockDevice>(
    disk: &mut D,
    fat: u64,
    cluster: u64,
    bytes: &mut [u8; SECTOR_SIZE],
    cached: &mut Option<u64>,
) -> Result<u64, Error> {
    let sector = fat + cluster * 4 / 512;
    if *cached != Some(sector) {
        disk.read_sectors(sector, bytes).map_err(Error::Block)?;
        *cached = Some(sector);
    }
    let at = (cluster * 4 % 512) as usize;
    Ok(
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as u64
            & 0x0fff_ffff,
    )
}
