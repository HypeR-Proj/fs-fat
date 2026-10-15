// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::panic)]
use std::prelude::v1::*;
use std::{format, panic, vec};
fn require_ok<T, E: core::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("unexpected error: {error:?}"),
    }
}
use crate::block::{BlockDevice, Error as BlockError, SECTOR_SIZE, WriteRequest, validate_range};
use crate::{Error, FatVolume};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    sectors: BTreeMap<u64, [u8; 512]>,
    reads: usize,
    writes: usize,
    flushes: usize,
    fail_flush: bool,
    fail_reads: bool,
    fail_writes: bool,
    fail_write_number: Option<usize>,
    largest_write: usize,
    readonly: bool,
}
#[derive(Clone)]
struct Disk(Arc<Mutex<State>>);
impl Disk {
    fn fresh() -> Self {
        let mut state = State::default();
        let mut boot = [0; 512];
        boot[0..3].copy_from_slice(&[0xeb, 0x58, 0x90]);
        boot[3..11].copy_from_slice(b"HYPER   ");
        boot[11..13].copy_from_slice(&512u16.to_le_bytes());
        boot[13] = 1;
        boot[14..16].copy_from_slice(&32u16.to_le_bytes());
        boot[16] = 2;
        boot[21] = 0xf8;
        boot[32..36].copy_from_slice(&70000u32.to_le_bytes());
        boot[36..40].copy_from_slice(&600u32.to_le_bytes());
        boot[44..48].copy_from_slice(&2u32.to_le_bytes());
        boot[48..50].copy_from_slice(&1u16.to_le_bytes());
        boot[50..52].copy_from_slice(&6u16.to_le_bytes());
        boot[64] = 0x80;
        boot[66] = 0x29;
        boot[71..82].copy_from_slice(b"HYPER DATA ");
        boot[82..90].copy_from_slice(b"FAT32   ");
        boot[510..512].copy_from_slice(&[0x55, 0xaa]);
        state.sectors.insert(0, boot);
        state.sectors.insert(6, boot);
        let mut info = [0; 512];
        info[..4].copy_from_slice(&0x41615252u32.to_le_bytes());
        info[484..488].copy_from_slice(&0x61417272u32.to_le_bytes());
        info[488..492].copy_from_slice(&u32::MAX.to_le_bytes());
        info[492..496].copy_from_slice(&u32::MAX.to_le_bytes());
        info[508..512].copy_from_slice(&0xaa550000u32.to_le_bytes());
        state.sectors.insert(1, info);
        let mut fat = [0; 512];
        fat[..4].copy_from_slice(&0x0ffffff8u32.to_le_bytes());
        fat[4..8].copy_from_slice(&0xffffffffu32.to_le_bytes());
        fat[8..12].copy_from_slice(&0x0fffffffu32.to_le_bytes());
        state.sectors.insert(32, fat);
        state.sectors.insert(632, fat);
        Self(Arc::new(Mutex::new(state)))
    }
}
impl BlockDevice for Disk {
    fn is_read_only(&self) -> bool {
        require_ok(self.0.lock()).readonly
    }

    fn sector_count(&self) -> u64 {
        70000
    }
    fn read_sectors(&mut self, first: u64, output: &mut [u8]) -> Result<(), BlockError> {
        validate_range(self.sector_count(), first, output.len())?;
        let mut state = require_ok(self.0.lock());
        state.reads += 1;
        if state.fail_reads {
            return Err(BlockError::Io);
        }
        for (i, sector) in output.chunks_exact_mut(512).enumerate() {
            sector.copy_from_slice(state.sectors.get(&(first + i as u64)).unwrap_or(&[0; 512]));
        }
        Ok(())
    }
    fn write_sectors(&mut self, first: u64, input: &[u8]) -> Result<(), BlockError> {
        validate_range(self.sector_count(), first, input.len())?;
        let mut state = require_ok(self.0.lock());
        state.writes += 1;
        state.largest_write = state.largest_write.max(input.len());
        if state.readonly {
            return Err(BlockError::ReadOnly);
        }
        for (i, sector) in input.chunks_exact(512).enumerate() {
            let mut copy = [0; 512];
            copy.copy_from_slice(sector);
            state.sectors.insert(first + i as u64, copy);
        }
        // Model a device that reports failure after touching the media.
        if state.fail_writes || state.fail_write_number == Some(state.writes) {
            return Err(BlockError::Io);
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<(), BlockError> {
        let mut state = require_ok(self.0.lock());
        state.flushes += 1;
        if state.fail_flush {
            Err(BlockError::Io)
        } else {
            Ok(())
        }
    }
}
#[test]
fn fat_first_read_builds_the_allocation_map_without_an_eof_seek() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("large", false));
    let contents: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    require_ok(fs.write_at("large", 0, &contents));
    require_ok(fs.sync());
    drop(fs);
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    let before = require_ok(disk.0.lock()).reads;
    let mut byte = [0];
    assert_eq!(require_ok(fs.read_at("large", 17, &mut byte)), 1);
    assert_eq!(byte, [contents[17]]);
    // Eight FAT windows plus directory/data windows: a preliminary seek to
    // EOF would traverse and evict this chain before mapping it a second time.
    let reads = require_ok(disk.0.lock()).reads - before;
    assert!(
        reads <= 12,
        "first read walked the FAT twice: {reads} requests"
    );
    let last = contents.len() - 1;
    assert_eq!(require_ok(fs.read_at("large", last as u64, &mut byte)), 1);
    assert_eq!(byte, [contents[last]]);
}

#[test]
fn fat_writes_coalesce_clusters_and_complete_before_returning() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("large", false));
    let contents: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    let before = require_ok(disk.0.lock()).writes;
    assert_eq!(
        require_ok(fs.write_at("large", 0, &contents)),
        contents.len()
    );
    {
        let state = require_ok(disk.0.lock());
        let writes = state.writes - before;
        assert!(
            writes <= 16,
            "256 KiB write issued {writes} device requests"
        );
        assert!(state.largest_write >= 64 * 1024);
        assert_eq!(
            state.flushes, 0,
            "ordinary write must not add a durability fence"
        );
    }
    // Drop does no I/O. Re-admitting the on-device view proves that success
    // did not leave unreported dirty sectors in the operation buffer.
    drop(fs);
    let mut fs = require_ok(FatVolume::mount(disk));
    let mut output = vec![0; contents.len()];
    assert_eq!(
        require_ok(fs.read_at("large", 0, &mut output)),
        output.len()
    );
    assert_eq!(output, contents);
}

#[test]
fn fat_write_buffer_eviction_and_partial_overwrites_preserve_other_sectors() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("large", false));
    let mut contents: Vec<u8> = (0..2 * 1024 * 1024 + 37).map(|i| (i % 251) as u8).collect();
    require_ok(fs.write_at("large", 0, &contents));
    for offset in [511, 128 * 1024 - 7, 1024 * 1024 + 511] {
        let replacement = [0xa7; 1031];
        require_ok(fs.write_at("large", offset as u64, &replacement));
        contents[offset..offset + replacement.len()].copy_from_slice(&replacement);
    }
    require_ok(fs.sync());
    drop(fs);
    let mut fs = require_ok(FatVolume::mount(disk));
    let mut output = vec![0; contents.len()];
    assert_eq!(
        require_ok(fs.read_at("large", 0, &mut output)),
        output.len()
    );
    assert_eq!(output, contents);
}

#[test]
fn fat_sparse_and_contiguous_writes_preserve_overlay_across_drains() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("ranges", false));
    let mut contents = vec![0x53; 1024 * 1024 + 19];
    require_ok(fs.write_at("ranges", 0, &contents));
    // Cover single sectors, whole bitmap words and windows, and partial heads
    // and tails. Leave gaps whose old bytes must survive every operation.
    let mut seed = 0x17ab_u64;
    for length in [
        1,
        511,
        512,
        513,
        63 * 512,
        64 * 512,
        65 * 512,
        128 * 1024 + 1,
    ] {
        for _ in 0..8 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let offset = seed as usize % (contents.len() - length);
            let bytes = vec![(seed >> 32) as u8; length];
            require_ok(fs.write_at("ranges", offset as u64, &bytes));
            contents[offset..offset + length].copy_from_slice(&bytes);
        }
    }
    require_ok(fs.sync());
    drop(fs);
    let mut fs = require_ok(FatVolume::mount(disk));
    let mut output = vec![0; contents.len()];
    assert_eq!(
        require_ok(fs.read_at("ranges", 0, &mut output)),
        output.len()
    );
    assert_eq!(output, contents);
}

#[test]
fn fat_read_cache_refill_keeps_pending_neighbor_writes_after_drain() {
    for full_bytes in [20 * 1024, 2 * 1024 * 1024] {
        let disk = Disk::fresh();
        let mut fs = require_ok(FatVolume::mount(disk.clone()));
        require_ok(fs.create("refill", false));
        let mut contents: Vec<u8> = (0..full_bytes + 37).map(|i| (i % 251) as u8).collect();
        // The partial tail reads a previously uncached window containing a
        // neighboring full sector whose new contents are still buffered.
        require_ok(fs.write_at("refill", 0, &contents));
        let offset = full_bytes - SECTOR_SIZE + 17;
        // After the operation drains its writes, a read-modify-write must
        // preserve every other byte of that neighboring sector.
        require_ok(fs.write_at("refill", offset as u64, &[0xa7]));
        contents[offset] = 0xa7;
        require_ok(fs.sync());
        drop(fs);
        let mut fs = require_ok(FatVolume::mount(disk));
        let mut output = vec![0; contents.len()];
        assert_eq!(
            require_ok(fs.read_at("refill", 0, &mut output)),
            output.len()
        );
        let mismatch = output.iter().zip(&contents).position(|(a, b)| a != b);
        assert_eq!(
            mismatch, None,
            "corrupted {full_bytes}-byte write plus tail"
        );
    }
}

#[test]
fn fat_partial_buffer_drain_failure_stops_all_later_io() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.read_status());
    require_ok(fs.create("file", false));
    {
        let mut state = require_ok(disk.0.lock());
        state.fail_write_number = Some(state.writes + 2);
    }
    assert!(matches!(
        fs.write_at("file", 0, &vec![0x7a; 256 * 1024]),
        Err(Error::Block(BlockError::Io))
    ));
    let counts = {
        let state = require_ok(disk.0.lock());
        (state.reads, state.writes, state.flushes)
    };
    assert_eq!(fs.read_status(), Err(Error::Block(BlockError::Io)));
    assert!(matches!(fs.stat("file"), Err(Error::Block(BlockError::Io))));
    assert!(matches!(fs.sync(), Err(Error::Block(BlockError::Io))));
    drop(fs);
    let state = require_ok(disk.0.lock());
    assert_eq!((state.reads, state.writes, state.flushes), counts);
}

#[test]
fn fat_random_overwrites_reuse_chain_positions_and_forget_freed_clusters() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("large", false));
    let mut expected = vec![0x29; 4 * 1024 * 1024];
    require_ok(fs.write_at("large", 0, &expected));
    // Warm a chain larger than the metadata windows. Reopened File objects
    // must not traverse this FAT again for each random seek.
    let tail = expected.len() - SECTOR_SIZE;
    require_ok(fs.write_at("large", tail as u64, &[0x66; SECTOR_SIZE]));
    expected[tail..].fill(0x66);
    let reads = require_ok(disk.0.lock()).reads;
    for i in 0..32 {
        let offset = (i * 397 % (expected.len() / SECTOR_SIZE)) * SECTOR_SIZE;
        require_ok(fs.write_at("large", offset as u64, &[0x72; SECTOR_SIZE]));
        expected[offset..offset + SECTOR_SIZE].fill(0x72);
    }
    let metadata_reads = require_ok(disk.0.lock()).reads - reads;
    assert!(
        metadata_reads <= 64,
        "random writes repeated chain walks: {metadata_reads} reads"
    );
    let mut output = vec![0; expected.len()];
    require_ok(fs.read_at("large", 0, &mut output));
    assert_eq!(output, expected);

    require_ok(fs.resize("large", 512));
    require_ok(fs.create("other", false));
    let other = vec![0x91; expected.len()];
    require_ok(fs.write_at("other", 0, &other));
    // Extension now follows a different chain. Reopen it once more after it
    // grows, when EOF no longer clamps seeks into formerly cached positions.
    require_ok(fs.write_at("large", tail as u64, &[0x35; SECTOR_SIZE]));
    require_ok(fs.write_at("large", (tail - SECTOR_SIZE) as u64, &[0x47; SECTOR_SIZE]));
    require_ok(fs.sync());
    drop(fs);
    let mut fs = require_ok(FatVolume::mount(disk));
    require_ok(fs.read_at("other", 0, &mut output));
    assert_eq!(output, other);
    require_ok(fs.read_at("large", 0, &mut output));
    assert_eq!(&output[..512], &expected[..512]);
    assert!(
        output[512..tail - SECTOR_SIZE]
            .iter()
            .all(|&byte| byte == 0)
    );
    assert!(
        output[tail - SECTOR_SIZE..tail]
            .iter()
            .all(|&byte| byte == 0x47)
    );
    assert!(output[tail..].iter().all(|&byte| byte == 0x35));
}

#[test]
fn fat_tiny_file_reads_reuse_windows_and_refresh_after_writes() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("fit", false));
    require_ok(fs.write_at("fit", 0, &vec![0x42; 16 * 1024]));
    let mut byte = [0];
    for offset in [0, 511, 512, 4095, 8192] {
        assert_eq!(require_ok(fs.read_at("fit", offset, &mut byte)), 1);
        assert_eq!(byte, [0x42]);
    }
    let reads = require_ok(disk.0.lock()).reads;
    for _ in 0..32 {
        for offset in [0, 511, 512, 4095, 8192] {
            assert_eq!(require_ok(fs.read_at("fit", offset, &mut byte)), 1);
            assert_eq!(byte, [0x42]);
        }
    }
    assert_eq!(require_ok(disk.0.lock()).reads, reads);
    require_ok(fs.write_at("fit", 511, &[0x17]));
    assert_eq!(require_ok(fs.read_at("fit", 511, &mut byte)), 1);
    assert_eq!(byte, [0x17]);
}

#[test]
fn fat_failed_write_cannot_serve_cached_metadata_or_data() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("file", false));
    require_ok(fs.write_at("file", 0, b"contents"));
    let mut output = [0; 8];
    require_ok(fs.read_at("file", 0, &mut output));
    require_ok(fs.stat("file"));
    require_ok(disk.0.lock()).fail_writes = true;
    assert!(matches!(
        fs.resize("file", 1),
        Err(Error::Block(BlockError::Io))
    ));
    let reads = require_ok(disk.0.lock()).reads;
    assert_eq!(fs.read_status(), Err(Error::Block(BlockError::Io)));
    assert!(matches!(fs.stat("file"), Err(Error::Block(BlockError::Io))));
    assert!(matches!(
        fs.read_at("file", 0, &mut output),
        Err(Error::Block(BlockError::Io))
    ));
    assert_eq!(require_ok(disk.0.lock()).reads, reads);
}

#[test]
fn fat_metadata_windows_batch_cold_chain_and_retain_directory_sectors() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("images", true));
    require_ok(fs.create("images/guest.itb", false));
    let contents = vec![0x5a; 256 * 1024];
    require_ok(fs.write_at("images/guest.itb", 0, &contents));
    require_ok(fs.sync());
    drop(fs);
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    let before = require_ok(disk.0.lock()).reads;
    let mut output = vec![0; 64 * 1024];
    require_ok(fs.stat("images/guest.itb"));
    require_ok(fs.read_at("images/guest.itb", 0, &mut output));
    assert_eq!(output, contents[..output.len()]);
    let cold = require_ok(disk.0.lock()).reads - before;
    assert!(cold <= 8, "cold metadata scan took {cold} requests");
    let before = require_ok(disk.0.lock()).reads;
    for _ in 0..16 {
        assert_eq!(
            require_ok(fs.stat("images/guest.itb")).size,
            contents.len() as u64
        );
        require_ok(fs.read_at("images/guest.itb", 64 * 1024, &mut output));
    }
    assert_eq!(
        require_ok(disk.0.lock()).reads - before,
        16,
        "warm directory lookup must not add I/O to each data request"
    );
    require_ok(fs.resize("images/guest.itb", 123));
    assert_eq!(require_ok(fs.stat("images/guest.itb")).size, 123);
    require_ok(fs.sync());
    drop(fs);
    let mut fs = require_ok(FatVolume::mount(disk));
    assert_eq!(require_ok(fs.stat("images/guest.itb")).size, 123);
}

#[test]
fn fat_read_maps_coalesce_data_and_invalidate_on_mutation() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("image", false));
    let contents: Vec<u8> = (0..256 * 1024).map(|index| (index % 251) as u8).collect();
    assert_eq!(
        require_ok(fs.write_at("image", 0, &contents)),
        contents.len()
    );
    let mut byte = [0];
    require_ok(fs.read_at("image", 0, &mut byte));
    let before = require_ok(disk.0.lock()).reads;
    let mut output = vec![0; 64 * 1024];
    assert_eq!(
        require_ok(fs.read_at("image", 128 * 1024, &mut output)),
        output.len()
    );
    assert_eq!(output, contents[128 * 1024..192 * 1024]);
    assert_eq!(
        require_ok(disk.0.lock()).reads - before,
        1,
        "one contiguous data transfer, no FAT walk"
    );
    let mut unaligned = [0; 1031];
    require_ok(fs.read_at("image", 511, &mut unaligned));
    assert_eq!(unaligned, contents[511..1542]);
    require_ok(fs.resize("image", 0));
    require_ok(fs.create("reuse", false));
    require_ok(fs.write_at("reuse", 0, &[0xaa; 4096]));
    assert_eq!(require_ok(fs.read_at("image", 0, &mut byte)), 0);
    require_ok(fs.write_at("image", 0, b"new"));
    let mut new = [0; 3];
    require_ok(fs.read_at("image", 0, &mut new));
    assert_eq!(&new, b"new");
    require_ok(fs.rename("image", "renamed"));
    assert!(matches!(
        fs.read_at("image", 0, &mut byte),
        Err(Error::Missing)
    ));
    require_ok(fs.read_at("renamed", 0, &mut new));
    assert_eq!(&new, b"new");
    require_ok(fs.remove("renamed"));
    require_ok(fs.create("renamed", false));
    assert_eq!(require_ok(fs.read_at("renamed", 0, &mut byte)), 0);
}

#[test]
fn fat_fragmented_reads_and_writes_remain_correct_beyond_mapping_cache_limit() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk));
    require_ok(fs.create("a", false));
    require_ok(fs.create("b", false));
    for index in 0..140u64 {
        require_ok(fs.write_at("a", index * 512, &[index as u8; 512]));
        require_ok(fs.write_at("b", index * 512, &[0xee; 512]));
    }
    let mut data = vec![0; 140 * 512];
    for _ in 0..2 {
        assert_eq!(require_ok(fs.read_at("a", 0, &mut data)), data.len());
        for (index, sector) in data.chunks_exact(512).enumerate() {
            assert!(sector.iter().all(|byte| *byte == index as u8));
        }
    }
    for index in (0..140u64).rev() {
        require_ok(fs.write_at("a", index * 512 + 7, &[0x37; 31]));
    }
    require_ok(fs.read_at("a", 0, &mut data));
    for (index, sector) in data.chunks_exact(512).enumerate() {
        assert!(sector[..7].iter().all(|&byte| byte == index as u8));
        assert!(sector[7..38].iter().all(|&byte| byte == 0x37));
        assert!(sector[38..].iter().all(|&byte| byte == index as u8));
    }
    require_ok(fs.read_at("b", 0, &mut data));
    assert!(data.iter().all(|&byte| byte == 0xee));
}

#[test]
fn fat_file_timestamps_use_utc_and_survive_remount() {
    use crate::Timestamp;
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount_with_clock(disk.clone(), || {
        Timestamp::new(1709251201, 120_000_000)
    }));
    require_ok(fs.create("time.txt", false));
    let created = require_ok(fs.stat("time.txt")).created;
    assert_eq!(created, Timestamp::new(1709251201, 120_000_000));
    // 2000-02-29 is a leap day; modification time has FAT's two-second precision.
    let update = Timestamp::new(951827697, 123_000_000);
    require_ok(fs.set_times("time.txt", update, update));
    require_ok(fs.sync());
    drop(fs);
    let mut fs = require_ok(FatVolume::mount(disk));
    let entry = require_ok(fs.stat("time.txt"));
    assert_eq!(entry.created, created);
    assert_eq!(entry.modified, Timestamp::new(951827696, 0));
    assert_eq!(entry.accessed, Timestamp::new(951782400, 0));
    assert_eq!(
        fs.set_times("time.txt", None, Timestamp::new(0, 0)),
        Err(Error::InvalidInput)
    );
    assert_eq!(require_ok(fs.stat("time.txt")).modified, entry.modified);
    // End of the representable FAT range, including the non-leap year 2100.
    require_ok(fs.set_times("time.txt", None, Timestamp::new(4354819199, 0)));
    assert_eq!(
        require_ok(fs.stat("time.txt")).modified,
        Timestamp::new(4354819198, 0)
    );
    assert_eq!(
        fs.set_times("time.txt", None, Timestamp::new(4354819200, 0)),
        Err(Error::InvalidInput)
    );
}

#[test]
fn fat_persists_long_names_sparse_writes_and_resize_across_mounts() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("virtual machines", true));
    require_ok(fs.create("virtual machines/configuration.json", false));
    require_ok(fs.write_at("virtual machines/configuration.json", 700, b"persisted"));
    require_ok(fs.sync());
    drop(fs);
    let mut fs = require_ok(FatVolume::mount(disk));
    let mut output = [0xff; 709];
    assert_eq!(
        require_ok(fs.read_at("virtual machines/configuration.json", 0, &mut output)),
        709
    );
    assert!(output[..700].iter().all(|b| *b == 0));
    assert_eq!(&output[700..], b"persisted");
    require_ok(fs.resize("virtual machines/configuration.json", 3));
    require_ok(fs.rename("virtual machines/configuration.json", "configuration.json"));
    require_ok(fs.remove("virtual machines"));
    assert_eq!(
        require_ok(fs.read_at("configuration.json", 0, &mut output)),
        3
    );
    require_ok(fs.remove("configuration.json"));
    require_ok(fs.sync());
}
#[test]
fn fat_drop_never_issues_device_io() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("test.txt", false));
    require_ok(fs.write_at("test.txt", 0, b"hello"));
    let before = {
        let state = require_ok(disk.0.lock());
        (state.reads, state.writes, state.flushes)
    };
    drop(fs);
    let state = require_ok(disk.0.lock());
    assert_eq!((state.reads, state.writes, state.flushes), before);
}
#[test]
fn fat_flush_error_is_reported_and_volume_is_not_reused() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("dirty.txt", false));
    require_ok(fs.write_at("dirty.txt", 0, b"pending"));
    require_ok(disk.0.lock()).fail_flush = true;
    assert_eq!(fs.sync(), Err(Error::Block(BlockError::Io)));
    let before = {
        let mut state = require_ok(disk.0.lock());
        assert_eq!(state.flushes, 1);
        // Recovery of the backend must not reopen a failed metadata view.
        state.fail_flush = false;
        (state.reads, state.writes, state.flushes)
    };
    assert_eq!(fs.read_status(), Err(Error::Block(BlockError::Io)));
    assert!(matches!(
        directory_entry(&mut fs, "", 0),
        Err(Error::Block(BlockError::Io))
    ));
    assert_eq!(
        fs.write_at("dirty.txt", 0, b"lost"),
        Err(Error::Block(BlockError::Io))
    );
    assert_eq!(fs.sync(), Err(Error::Block(BlockError::Io)));
    drop(fs);
    let state = require_ok(disk.0.lock());
    assert_eq!((state.reads, state.writes, state.flushes), before);
}

#[test]
fn fat_sync_retains_clean_view_and_persists_later_allocations() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("continued.txt", false));
    require_ok(fs.write_at("continued.txt", 0, b"first"));
    require_ok(fs.sync());
    let before = {
        let state = require_ok(disk.0.lock());
        assert_eq!(state.sectors[&0][0x41] & 1, 0);
        (state.reads, state.writes, state.flushes)
    };
    require_ok(fs.sync());
    {
        let state = require_ok(disk.0.lock());
        // A clean sync fences the device without reparsing the filesystem.
        assert_eq!(
            (state.reads, state.writes, state.flushes),
            (before.0, before.1, before.2 + 1)
        );
    }
    require_ok(fs.write_at("continued.txt", 700, b"second"));
    assert_ne!(require_ok(disk.0.lock()).sectors[&0][0x41] & 1, 0);
    require_ok(fs.create("later.txt", false));
    require_ok(fs.write_at("later.txt", 0, &[0x5a; 1024]));
    require_ok(fs.sync());
    assert_eq!(require_ok(disk.0.lock()).sectors[&0][0x41] & 1, 0);
    drop(fs);
    let mut fs = require_ok(FatVolume::mount(disk));
    let mut bytes = [0xff; 706];
    assert_eq!(
        require_ok(fs.read_at("continued.txt", 0, &mut bytes)),
        bytes.len()
    );
    assert_eq!(&bytes[..5], b"first");
    assert!(bytes[5..700].iter().all(|byte| *byte == 0));
    assert_eq!(&bytes[700..], b"second");
    let mut later = [0; 1024];
    assert_eq!(
        require_ok(fs.read_at("later.txt", 0, &mut later)),
        later.len()
    );
    assert_eq!(later, [0x5a; 1024]);
}
#[test]
fn fat_rejects_cyclic_root_and_out_of_bounds_directory_cluster() {
    let disk = Disk::fresh();
    {
        let mut state = require_ok(disk.0.lock());
        let fat = require_ok(state.sectors.get_mut(&32).ok_or("missing FAT sector"));
        fat[8..12].copy_from_slice(&2u32.to_le_bytes());
    }
    assert!(matches!(FatVolume::mount(disk), Err(Error::Corrupt)));
    let disk = Disk::fresh();
    {
        let mut state = require_ok(disk.0.lock());
        let mut root = [0; 512];
        root[..11].copy_from_slice(b"BAD     TXT");
        root[20..22].copy_from_slice(&0xffffu16.to_le_bytes());
        root[26..28].copy_from_slice(&0xffffu16.to_le_bytes());
        state.sectors.insert(1232, root);
    }
    assert!(matches!(FatVolume::mount(disk), Err(Error::Corrupt)));
}
#[test]
fn block_ranges_reject_overflow_and_partial_sectors() {
    assert_eq!(
        validate_range(10, 9, 2 * SECTOR_SIZE),
        Err(BlockError::InvalidRange)
    );
    assert_eq!(
        validate_range(u64::MAX, u64::MAX, SECTOR_SIZE),
        Err(BlockError::InvalidRange)
    );
    assert_eq!(validate_range(10, 0, 1), Err(BlockError::InvalidRange));
    assert_eq!(validate_range(10, 10, 0), Ok(()));
}

#[test]
fn block_write_batches_validate_every_range_before_modifying_media() {
    let mut disk = Disk::fresh();
    let sector = [0x5a; SECTOR_SIZE];
    let first = WriteRequest {
        first: 20,
        bytes: &sector,
    };
    for invalid in [
        WriteRequest {
            first: 70000,
            bytes: &sector,
        },
        WriteRequest {
            first: u64::MAX,
            bytes: &sector,
        },
        WriteRequest {
            first: 21,
            bytes: &sector[..1],
        },
        WriteRequest {
            first: 20,
            bytes: &sector,
        },
    ] {
        assert_eq!(
            disk.write_batch(&[first, invalid]),
            Err(BlockError::InvalidRange)
        );
        assert_eq!(require_ok(disk.0.lock()).writes, 0);
    }
    require_ok(disk.write_batch(&[
        first,
        WriteRequest {
            first: 19,
            bytes: &sector,
        },
        WriteRequest {
            first: 21,
            bytes: &sector,
        },
        WriteRequest {
            first: 20,
            bytes: &[],
        },
        WriteRequest {
            first: 70000,
            bytes: &[],
        },
    ]));
    assert_eq!(require_ok(disk.0.lock()).writes, 3);
    let mut output = [0; 3 * SECTOR_SIZE];
    require_ok(disk.read_sectors(19, &mut output));
    assert_eq!(output, [0x5a; 3 * SECTOR_SIZE]);
}

#[test]
fn fat_metadata_reads_do_not_flush_the_physical_device() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("config.json", false));
    require_ok(fs.write_at("config.json", 0, b"{}"));
    let _ = require_ok(fs.stat("config.json"));
    let mut data = [0; 2];
    require_ok(fs.read_at("config.json", 0, &mut data));
    assert_eq!(require_ok(disk.0.lock()).flushes, 0);
    require_ok(fs.sync());
    assert_eq!(require_ok(disk.0.lock()).flushes, 1);
}

#[test]
fn fat_create_is_exclusive_and_zero_write_does_not_extend() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk));
    require_ok(fs.create("config.json", false));
    assert_eq!(fs.create("config.json", false), Err(Error::Exists));
    assert_eq!(require_ok(fs.write_at("config.json", 512, &[])), 0);
    assert_eq!(require_ok(fs.stat("config.json")).size, 0);
    let mut byte = [0];
    assert_eq!(
        require_ok(fs.read_at("config.json", u64::MAX, &mut byte)),
        0
    );
}

#[test]
fn fat_rejects_file_chains_crosslinked_to_directory() {
    let disk = Disk::fresh();
    {
        let mut state = require_ok(disk.0.lock());
        let mut root = [0; 512];
        root[..11].copy_from_slice(b"BAD     TXT");
        root[26..28].copy_from_slice(&2u16.to_le_bytes());
        root[28..32].copy_from_slice(&512u32.to_le_bytes());
        state.sectors.insert(1232, root);
    }
    assert!(matches!(FatVolume::mount(disk), Err(Error::Corrupt)));
}

#[test]
fn fat_rename_full_destination_preserves_source() {
    let disk = Disk::fresh();
    {
        let mut state = require_ok(disk.0.lock());
        let mut allocated = [0u8; 512];
        for value in allocated.chunks_exact_mut(4) {
            value.copy_from_slice(&0x0fffffffu32.to_le_bytes());
        }
        for sector in 32..1232 {
            state.sectors.insert(sector, allocated);
        }
        let mut root = [0u8; 512];
        root[..11].copy_from_slice(b"OLD     TXT");
        root[11] = 0x20;
        root[32..43].copy_from_slice(b"DEST       ");
        root[43] = 0x10;
        root[58..60].copy_from_slice(&3u16.to_le_bytes());
        state.sectors.insert(1232, root);
        let mut destination = [0u8; 512];
        for (index, entry) in destination.chunks_exact_mut(32).enumerate() {
            let name = format!("F{index:07}TXT");
            entry[..11].copy_from_slice(name.as_bytes());
            entry[11] = 0x20;
        }
        state.sectors.insert(1233, destination);
    }
    let mut fs = require_ok(FatVolume::mount(disk));
    assert_eq!(
        fs.rename("OLD.TXT", "DEST/long-destination-name.txt"),
        Err(Error::NoSpace)
    );
    assert!(fs.stat("OLD.TXT").is_ok(), "ENOSPC must not unlink source");
}

#[test]
fn fat_directory_move_updates_on_disk_parent() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("a", true));
    require_ok(fs.create("b", true));
    require_ok(fs.create("a/child", true));
    require_ok(fs.rename("a/child", "b/child"));
    assert!(matches!(fs.stat("a/child"), Err(Error::Missing)));
    require_ok(fs.sync());
    let state = require_ok(disk.0.lock());
    let cluster = |sector: u64, name: &[u8; 11]| {
        state.sectors[&sector]
            .chunks_exact(32)
            .find(|entry| &entry[..11] == name)
            .map(|entry| {
                u32::from(u16::from_le_bytes([entry[26], entry[27]]))
                    | (u32::from(u16::from_le_bytes([entry[20], entry[21]])) << 16)
            })
            .unwrap_or(0)
    };
    let parent = cluster(1232, b"B          ");
    assert!(parent >= 2);
    let child = cluster(1232 + u64::from(parent) - 2, b"CHILD      ");
    assert!(child >= 2);
    assert_eq!(cluster(1232 + u64::from(child) - 2, b"..         "), parent);
    drop(state);
    require_ok(fs.rename("b/child", "child"));
    require_ok(fs.sync());
    drop(fs);
    let _remounted = require_ok(FatVolume::mount(disk.clone()));
    let state = require_ok(disk.0.lock());
    let moved = &state.sectors[&(1232 + u64::from(child) - 2)];
    assert_eq!(&moved[32..43], b"..         ");
    assert_eq!(&moved[52..54], &[0, 0]);
    assert_eq!(&moved[58..60], &[0, 0]);
}

#[test]
fn fat_deep_paths_do_not_recurse_on_the_kernel_stack() {
    let worker = require_ok(
        std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(|| {
                let mut fs = require_ok(FatVolume::mount(Disk::fresh()));
                let mut directory = String::new();
                for _ in 0..192 {
                    if !directory.is_empty() {
                        directory.push('/');
                    }
                    directory.push('a');
                    require_ok(fs.create(&directory, true));
                }
                let file = format!("{directory}/file");
                let renamed = format!("{directory}/renamed");
                require_ok(fs.create(&file, false));
                assert_eq!(require_ok(fs.write_at(&file, 0, b"deep")), 4);
                require_ok(fs.rename(&file, &renamed));
                let mut output = [0; 4];
                assert_eq!(require_ok(fs.read_at(&renamed, 0, &mut output)), 4);
                assert_eq!(&output, b"deep");
                require_ok(fs.remove(&renamed));
                require_ok(fs.remove(&directory));
            }),
    );
    require_ok(worker.join());
}

#[test]
fn fat_read_only_rejections_preserve_read_access_without_device_writes() {
    let disk = Disk::fresh();
    let mut volume = require_ok(FatVolume::mount(disk.clone()));
    require_ok(volume.create("keep.txt", false));
    require_ok(volume.write_at("keep.txt", 0, b"keep"));
    require_ok(volume.sync());
    drop(volume);
    let (writes, flushes) = {
        let mut state = require_ok(disk.0.lock());
        state.readonly = true;
        (state.writes, state.flushes)
    };
    let mut volume = require_ok(FatVolume::mount(disk.clone()));
    assert!(volume.is_read_only());
    assert!(require_ok(volume.stat("")).read_only);
    assert!(require_ok(volume.stat("keep.txt")).read_only);
    let error = Error::Block(BlockError::ReadOnly);
    assert_eq!(volume.write_at("keep.txt", 0, b"bad"), Err(error));
    assert_eq!(volume.create("new.txt", false), Err(error));
    assert_eq!(volume.create("directory", true), Err(error));
    assert_eq!(volume.remove("keep.txt"), Err(error));
    assert_eq!(volume.rename("keep.txt", "lost.txt"), Err(error));
    assert_eq!(volume.resize("keep.txt", 0), Err(error));
    assert_eq!(volume.set_times("keep.txt", None, None), Err(error));
    let mut bytes = [0; 4];
    assert_eq!(require_ok(volume.read_at("keep.txt", 0, &mut bytes)), 4);
    assert_eq!(&bytes, b"keep");
    require_ok(volume.sync());
    drop(volume);
    let state = require_ok(disk.0.lock());
    assert_eq!(state.writes, writes);
    assert_eq!(state.flushes, flushes);
}

#[test]
fn fat_short_alias_is_canonical_and_exclusive_create_preserves_contents() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("longfilename.txt", false));
    require_ok(fs.write_at("longfilename.txt", 0, b"original"));
    let alias = first_root_alias(&disk);
    assert_eq!(require_ok(fs.stat(&alias)).name(), "longfilename.txt");
    assert_eq!(fs.create(&alias, false), Err(Error::Exists));
    let mut bytes = [0; 8];
    assert_eq!(require_ok(fs.read_at(&alias, 0, &mut bytes)), 8);
    assert_eq!(&bytes, b"original");
    require_ok(fs.sync());
    drop(fs);
    let mut fs = require_ok(FatVolume::mount(disk));
    assert_eq!(require_ok(fs.stat(&alias)).name(), "longfilename.txt");
    assert_eq!(require_ok(fs.read_at("longfilename.txt", 0, &mut bytes)), 8);
    assert_eq!(&bytes, b"original");
}

#[test]
fn fat_mkdir_full_parent_returns_unpublished_cluster() {
    let disk = Disk::fresh();
    {
        let mut state = require_ok(disk.0.lock());
        let mut allocated = [0u8; 512];
        for value in allocated.chunks_exact_mut(4) {
            value.copy_from_slice(&0x0fffffffu32.to_le_bytes());
        }
        for sector in 32..1232 {
            state.sectors.insert(sector, allocated);
        }
        for sector in [32, 632] {
            require_ok(state.sectors.get_mut(&sector).ok_or("FAT"))[12..16].fill(0);
        }
        let mut root = [0; 512];
        for (index, entry) in root.chunks_exact_mut(32).enumerate() {
            entry[..11].copy_from_slice(format!("F{index:07}TXT").as_bytes());
            entry[11] = 0x20;
        }
        state.sectors.insert(1232, root);
    }
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    for _ in 0..2 {
        assert_eq!(fs.create("child", true), Err(Error::NoSpace));
        assert!(matches!(fs.stat("child"), Err(Error::Missing)));
        let state = require_ok(disk.0.lock());
        for sector in [32, 632] {
            assert_eq!(
                &require_ok(state.sectors.get(&sector).ok_or("FAT"))[12..16],
                &[0; 4]
            );
        }
    }
    require_ok(fs.sync());
    drop(fs);
    let mut fs = require_ok(FatVolume::mount(disk));
    assert!(matches!(fs.stat("child"), Err(Error::Missing)));
    assert!(fs.stat("F0000000.TXT").is_ok());
}

#[test]
fn fat_iterator_preserves_names_end_and_io_errors() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(fs.create("deleted-long-name.txt", false));
    require_ok(fs.create("SHORT.TXT", false));
    let unicode = "é".repeat(100) + ".txt";
    require_ok(fs.create(&unicode, false));
    require_ok(fs.remove("deleted-long-name.txt"));
    let mut names = Vec::new();
    for index in 0..3 {
        if let Some(entry) = require_ok(directory_entry(&mut fs, "", index)) {
            names.push(entry.name().to_owned());
        } else {
            assert_eq!(index, 2);
        }
    }
    assert_eq!(names, ["SHORT.TXT".to_owned(), unicode]);
    assert!(require_ok(directory_entry(&mut fs, "", 3)).is_none());
    require_ok(fs.sync());
    drop(fs);
    // Force an actual directory cache miss before injecting the I/O error.
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    require_ok(disk.0.lock()).fail_reads = true;
    assert!(matches!(
        directory_entry(&mut fs, "", 0),
        Err(Error::Block(BlockError::Io))
    ));
    let reads = require_ok(disk.0.lock()).reads;
    assert!(matches!(
        directory_entry(&mut fs, "", 0),
        Err(Error::Block(BlockError::Io))
    ));
    assert_eq!(require_ok(disk.0.lock()).reads, reads);
}

// Decode the first actual on-disk SFN instead of assuming its collision suffix.
fn first_root_alias(disk: &Disk) -> String {
    let state = require_ok(disk.0.lock());
    let root = require_ok(state.sectors.get(&1232).ok_or("root sector"));
    let short = require_ok(
        root.chunks_exact(32)
            .find(|e| e[0] != 0 && e[0] != 0xe5 && e[11] != 0xf)
            .ok_or("SFN"),
    );
    let base = require_ok(std::str::from_utf8(&short[..8])).trim_end();
    let ext = require_ok(std::str::from_utf8(&short[8..11])).trim_end();
    format!("{base}.{ext}")
}

#[test]
fn fat_compact_mutation_cursor_preserves_collisions_aliases_and_data() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    for index in 0..12u8 {
        let name = format!("long-collision-name-{index}.txt");
        require_ok(fs.create(&name, false));
        require_ok(fs.write_at(&name, 0, &[index; 600]));
    }
    let alias = first_root_alias(&disk);
    assert_eq!(
        fs.rename(&alias, "long-collision-name-1.txt"),
        Err(Error::Exists)
    );
    require_ok(fs.rename(&alias, "renamed-long-name.txt"));
    assert!(matches!(
        fs.stat("long-collision-name-0.txt"),
        Err(Error::Missing)
    ));
    for index in (2..12u8).step_by(2) {
        require_ok(fs.remove(&format!("long-collision-name-{index}.txt")));
    }
    // Reuse deleted directory slots and freed clusters with different metadata.
    require_ok(fs.create("replacement-long-name.txt", false));
    require_ok(fs.write_at("replacement-long-name.txt", 0, &[99; 600]));
    require_ok(fs.sync());
    drop(fs);
    let mut fs = require_ok(FatVolume::mount(disk));
    let mut bytes = [0; 600];
    for (name, value) in [
        ("renamed-long-name.txt", 0),
        ("replacement-long-name.txt", 99),
    ] {
        assert_eq!(require_ok(fs.read_at(name, 0, &mut bytes)), bytes.len());
        assert_eq!(bytes, [value; 600]);
    }
    for index in (1..12u8).step_by(2) {
        assert_eq!(
            require_ok(fs.read_at(&format!("long-collision-name-{index}.txt"), 0, &mut bytes)),
            bytes.len()
        );
        assert_eq!(bytes, [index; 600]);
    }
}

#[test]
fn fat_inplace_lfn_parser_preserves_malformed_sequence_behavior() {
    for defect in ["checksum", "order", "deleted", "end", "unicode"] {
        let disk = Disk::fresh();
        let mut fs = require_ok(FatVolume::mount(disk.clone()));
        require_ok(fs.create("longfilename.txt", false));
        require_ok(fs.write_at("longfilename.txt", 0, b"original"));
        require_ok(fs.create("NEXT.TXT", false));
        require_ok(fs.sync());
        let alias = first_root_alias(&disk);
        drop(fs);
        {
            let mut state = require_ok(disk.0.lock());
            let root = require_ok(state.sectors.get_mut(&1232).ok_or("root sector"));
            // This name occupies two LFN slots followed by its authoritative SFN.
            assert_eq!(root[0], 0x42);
            assert_eq!(root[32], 1);
            assert_ne!(root[64 + 11], 0xf);
            match defect {
                "checksum" => root[13] ^= 1,
                "order" => root[32] = 2,
                "deleted" => root[32] = 0xe5,
                "end" => root[64] = 0,
                "unicode" => root[33..35].copy_from_slice(&0xd800u16.to_le_bytes()),
                _ => panic!("unexpected test defect"),
            }
        }
        let mut fs = require_ok(FatVolume::mount(disk.clone()));
        if defect == "end" {
            assert!(require_ok(directory_entry(&mut fs, "", 0)).is_none());
        } else if defect == "unicode" {
            let writes = require_ok(disk.0.lock()).writes;
            assert!(matches!(
                fs.create("NEXT.TXT", false),
                Err(Error::Block(BlockError::Corrupt))
            ));
            assert_eq!(require_ok(disk.0.lock()).writes, writes);
            assert!(matches!(
                directory_entry(&mut fs, "", 0),
                Err(Error::Block(BlockError::Corrupt))
            ));
        } else {
            assert!(matches!(fs.create(&alias, false), Err(Error::Exists)));
            let entry = require_ok(
                require_ok(directory_entry(&mut fs, "", 0)).ok_or("missing short entry"),
            );
            assert_eq!(entry.name(), alias);
            let next =
                require_ok(require_ok(directory_entry(&mut fs, "", 1)).ok_or("missing next entry"));
            assert_eq!(next.name(), "NEXT.TXT");
            let mut data = [0; 8];
            assert_eq!(require_ok(fs.read_at(&alias, 0, &mut data)), 8);
            assert_eq!(&data, b"original");
        }
    }
}

#[test]
fn fat_restarted_lfn_chain_cannot_retain_an_older_long_suffix() {
    let disk = Disk::fresh();
    let mut fs = require_ok(FatVolume::mount(disk.clone()));
    let old = "old-long-name-".repeat(7) + ".txt";
    require_ok(fs.create(&old, false));
    require_ok(fs.create("new-name.txt", false));
    require_ok(fs.write_at("new-name.txt", 0, b"new"));
    require_ok(fs.sync());
    drop(fs);
    {
        let mut state = require_ok(disk.0.lock());
        let root = require_ok(state.sectors.get_mut(&1232).ok_or("root sector"));
        let old_short = require_ok(
            root.chunks_exact(32)
                .position(|entry| entry[11] != 0xf)
                .ok_or("old SFN"),
        );
        let mut replacement = [0; 64];
        replacement.copy_from_slice(&root[(old_short + 1) * 32..(old_short + 3) * 32]);
        assert_eq!(replacement[0], 0x41);
        assert_ne!(replacement[32 + 11], 0xf);
        // Keep the old chain's first LAST fragment, then begin a different,
        // shorter valid chain before the old sequence can finish.
        assert!(root[0] > 0x41);
        root[32..96].copy_from_slice(&replacement);
        root[96] = 0;
    }
    let mut fs = require_ok(FatVolume::mount(disk));
    let entry = require_ok(require_ok(directory_entry(&mut fs, "", 0)).ok_or("new entry"));
    assert_eq!(entry.name(), "new-name.txt");
    assert!(require_ok(directory_entry(&mut fs, "", 1)).is_none());
    let mut data = [0; 3];
    assert_eq!(require_ok(fs.read_at("new-name.txt", 0, &mut data)), 3);
    assert_eq!(&data, b"new");
}

#[test]
fn directory_children_exclude_fat_dot_entries() {
    let mut fs = require_ok(FatVolume::mount(Disk::fresh()));
    require_ok(fs.create("parent", true));
    assert!(require_ok(directory_entry(&mut fs, "parent", 0)).is_none());
    require_ok(fs.create("parent/child.txt", false));
    require_ok(fs.create("parent/nested", true));
    assert_eq!(
        require_ok(require_ok(directory_entry(&mut fs, "parent", 0)).ok_or("first child")).name(),
        "child.txt"
    );
    assert_eq!(
        require_ok(require_ok(directory_entry(&mut fs, "parent", 1)).ok_or("second child")).name(),
        "nested"
    );
    assert!(require_ok(directory_entry(&mut fs, "parent", 2)).is_none());
    assert!(require_ok(directory_entry(&mut fs, "parent/nested", 0)).is_none());
}

// Deliberately interleave 24 fragmented chains, exceeding the FAT seek cache.
// Empty files need no data clusters; all work below is directory enumeration.
fn fragmented_directories(count: usize) -> Disk {
    let disk = Disk::fresh();
    let mut state = require_ok(disk.0.lock());
    let set_fat = |state: &mut State, cluster: u32, next: u32| {
        for base in [32, 632] {
            let sector = state
                .sectors
                .entry(base + u64::from(cluster / 128))
                .or_insert([0; 512]);
            let within = (cluster as usize % 128) * 4;
            sector[within..within + 4].copy_from_slice(&next.to_le_bytes());
        }
    };
    set_fat(&mut state, 2, 3);
    set_fat(&mut state, 3, 0x0fff_ffff);
    let mut root = [0; 1024];
    for directory in 0..24usize {
        let entry = &mut root[directory * 32..(directory + 1) * 32];
        entry[..11].copy_from_slice(format!("D{directory:07}   ").as_bytes());
        entry[11] = 0x10;
        entry[26..28].copy_from_slice(&(128 + directory as u16).to_le_bytes());
        let clusters = (count + 1).div_ceil(16);
        for index in 0..clusters {
            let cluster = 128 + directory as u32 + index as u32 * 24;
            set_fat(
                &mut state,
                cluster,
                if index + 1 < clusters {
                    cluster + 24
                } else {
                    0x0fff_ffff
                },
            );
            let mut bytes = [0; 512];
            for (slot, entry) in bytes.chunks_exact_mut(32).enumerate() {
                let item = index * 16 + slot;
                if item >= count {
                    break;
                }
                entry[..11].copy_from_slice(format!("F{item:07}TXT").as_bytes());
                entry[11] = 0x20;
            }
            state.sectors.insert(1232 + u64::from(cluster - 2), bytes);
        }
    }
    state
        .sectors
        .insert(1232, require_ok(root[..512].try_into()));
    state
        .sectors
        .insert(1233, require_ok(root[512..].try_into()));
    drop(state);
    disk
}

fn scan_fragmented_directories(count: usize) -> usize {
    use hyper_filesystem::protocol::{DIRECTORY_BATCH_BYTES, DirectoryBatch, DirectoryCursor};
    let disk = fragmented_directories(count);
    let mut volume = require_ok(FatVolume::mount(disk.clone()));
    let before = require_ok(disk.0.lock()).reads;
    let mut cursors = [Some(DirectoryCursor::START); 24];
    let mut seen = [0; 24];
    let mut output = [0; DIRECTORY_BATCH_BYTES];
    while cursors.iter().any(Option::is_some) {
        for (directory, cursor) in cursors.iter_mut().enumerate() {
            let Some(position) = *cursor else {
                continue;
            };
            let path = format!("D{directory:07}");
            let length = require_ok(volume.read_directory(&path, position, &mut output));
            let batch = require_ok(DirectoryBatch::decode(&output[..length]).ok_or("batch"));
            assert_eq!(batch.start_cursor(), position);
            for record in batch.records() {
                assert_eq!(record.name(), format!("F{:07}.TXT", seen[directory]));
                seen[directory] += 1;
                *cursor = Some(record.next_cursor());
            }
            if batch.eof() {
                *cursor = None;
            }
        }
    }
    assert_eq!(seen, [count; 24]);
    require_ok(disk.0.lock()).reads - before
}

#[test]
fn directory_batches_resume_interleaved_fragmented_chains_with_linear_reads() {
    let smaller = scan_fragmented_directories(256);
    let larger = scan_fragmented_directories(1024);
    assert!(
        larger <= smaller * 5 + 32,
        "directory I/O grew superlinearly: {smaller} -> {larger}"
    );
    assert!(
        larger < 1024,
        "too many directory device requests: {larger}"
    );
}

#[test]
fn directory_checkpoints_allow_unrelated_mutation_but_reject_cross_directory_reuse() {
    use hyper_filesystem::protocol::{DIRECTORY_MIN_BYTES, DirectoryBatch, DirectoryCursor};
    let mut volume = require_ok(FatVolume::mount(Disk::fresh()));
    require_ok(volume.create("source", true));
    require_ok(volume.create("destination", true));
    for index in 0..24 {
        require_ok(volume.create(&format!("source/{}-{index:03}.txt", "é".repeat(100)), false));
    }
    let mut output = [0; DIRECTORY_MIN_BYTES];
    let length = require_ok(volume.read_directory("source", DirectoryCursor::START, &mut output));
    let batch = require_ok(DirectoryBatch::decode(&output[..length]).ok_or("batch"));
    assert!(!batch.eof());
    let cursor = require_ok(batch.records().last().ok_or("record")).next_cursor();
    assert!(matches!(
        volume.read_directory("other", cursor, &mut output),
        Err(Error::InvalidInput)
    ));
    let length = require_ok(volume.read_directory("source", cursor, &mut output));
    let bytes = output[..length].to_vec();
    assert_eq!(
        require_ok(volume.read_directory("source", cursor, &mut output)),
        length
    );
    assert_eq!(&output[..length], bytes);
    require_ok(volume.create("destination/new", false));
    assert!(volume.read_directory("source", cursor, &mut output).is_ok());
}

// Small fixture assertions may select an ordinal locally; the media API only
// exposes resumable batches and never rescans an ordinal directory prefix.
fn directory_entry(
    volume: &mut FatVolume<Disk>,
    path: &str,
    mut index: usize,
) -> Result<Option<crate::Entry>, Error> {
    use hyper_filesystem::protocol::{
        DIRECTORY_BATCH_BYTES, DirectoryBatch, DirectoryCursor, Entry,
    };
    let mut cursor = DirectoryCursor::START;
    let mut bytes = [0; DIRECTORY_BATCH_BYTES];
    loop {
        let length = volume.read_directory(path, cursor, &mut bytes)?;
        let batch = DirectoryBatch::decode(&bytes[..length]).ok_or(Error::Corrupt)?;
        for record in batch.records() {
            if index == 0 {
                let mut entry = Entry::empty();
                record.entry_into(&mut entry);
                let directory = entry.directory();
                let read_only = entry.read_only();
                return Ok(Some(crate::Entry {
                    name: entry.name,
                    name_len: entry.name_len,
                    directory,
                    read_only,
                    size: entry.size,
                    created: entry.created,
                    accessed: entry.accessed,
                    modified: entry.modified,
                }));
            }
            index -= 1;
            cursor = record.next_cursor();
        }
        if batch.eof() {
            return Ok(None);
        }
    }
}

#[test]
fn drained_reopen_preserves_directory_cursor_and_written_data() {
    use hyper_filesystem::protocol::{DIRECTORY_MIN_BYTES, DirectoryBatch, DirectoryCursor};
    let mut volume = require_ok(FatVolume::mount(Disk::fresh()));
    for index in 0..40 {
        require_ok(volume.create(&format!("file-{index:03}.txt"), false));
    }
    require_ok(volume.write_at("file-000.txt", 0, b"survives provider replacement"));
    let mut output = [0; DIRECTORY_MIN_BYTES];
    let length = require_ok(volume.read_directory("", DirectoryCursor::START, &mut output));
    let batch = require_ok(DirectoryBatch::decode(&output[..length]).ok_or("batch"));
    assert!(!batch.eof());
    let cursor = require_ok(batch.records().last().ok_or("record")).next_cursor();
    let length = require_ok(volume.read_directory("", cursor, &mut output));
    let expected = output[..length].to_vec();
    let disk = require_ok(volume.into_device());
    let mut replacement = require_ok(FatVolume::mount(disk));
    let length = require_ok(replacement.read_directory("", cursor, &mut output));
    assert_eq!(&output[..length], expected);
    let mut contents = [0; 64];
    let length = require_ok(replacement.read_at("file-000.txt", 0, &mut contents));
    assert_eq!(&contents[..length], b"survives provider replacement");
}
