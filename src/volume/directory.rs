// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Stateless directory continuation: the kernel owns each checkpoint and its
//! authority and mutation fence. The caller invalidates checkpoints before
//! the scanned directory changes or its clusters can be reused.

use super::{BlockDevice, Disk, Entry, Error, FatVolume, validate_path};
use hyper_filesystem::protocol::{self, DirectoryBatchBuilder, DirectoryCursor};

const CURSOR_TAG: u64 = 0x4641_5431_0000_0000;

impl<D: BlockDevice> FatVolume<D> {
    /// Enumerates into a bounded wire batch without retaining an iterator.
    ///
    /// The caller must bind nonzero checkpoints to this mounted volume and
    /// directory, and invalidate them before changing that directory or moving
    /// an ancestor. Physical checkpoints are never application-supplied offsets.
    pub fn read_directory(
        &mut self,
        path: &str,
        cursor: DirectoryCursor,
        output: &mut [u8],
    ) -> Result<usize, Error> {
        validate_path(path)?;
        let mut batch = DirectoryBatchBuilder::new(output, cursor).ok_or(Error::InvalidInput)?;
        let path_key = path_key(path);
        let readonly = self.is_read_only();
        let position = if cursor.is_start() {
            None
        } else {
            if cursor.words[1] & !u64::from(u32::MAX) != CURSOR_TAG
                || cursor.words[2] != 0
                || cursor.words[3] != path_key
            {
                return Err(Error::InvalidInput);
            }
            Some(fatfs::DirectoryPosition {
                first_cluster: cursor.words[0] as u32,
                current_cluster: (cursor.words[0] >> 32) as u32,
                offset: cursor.words[1] as u32,
            })
        };
        self.run(|fs| {
            let mut iterator = if let Some(position) = position {
                fs.resume_directory(position).map_err(Error::from)?
            } else if path.is_empty() {
                fs.root_dir().iter()
            } else {
                fs.root_dir().open_dir(path).map_err(Error::from)?.iter()
            };
            let mut entry = Entry::empty();
            loop {
                let found = iterator
                    .visit_next(|found| {
                        if matches!(found.short_file_name_as_bytes(), b"." | b"..") {
                            return Ok(false);
                        }
                        Self::fill_entry(found, &mut entry, readonly)?;
                        Ok(true)
                    })
                    .map_err(Error::from)?;
                match found {
                    None => return batch.finish(true).ok_or(Error::Corrupt),
                    Some(Err(error)) => return Err(error),
                    Some(Ok(false)) => continue,
                    Some(Ok(true)) => {}
                }
                let position = iterator.position().map_err(Error::from)?;
                let next = DirectoryCursor {
                    words: [
                        u64::from(position.first_cluster)
                            | (u64::from(position.current_cluster) << 32),
                        CURSOR_TAG | u64::from(position.offset),
                        0,
                        path_key,
                    ],
                };
                if !push(&mut batch, &entry, next)? {
                    return batch.finish(false).ok_or(Error::Corrupt);
                }
            }
        })
    }
}

// This fingerprint detects accidental cross-directory reuse; authorization is
// supplied by the kernel stream, never by the hash or physical cluster number.
fn path_key(path: &str) -> u64 {
    path.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    })
}

#[inline(never)]
fn push(
    batch: &mut DirectoryBatchBuilder<'_>,
    entry: &Entry,
    next: DirectoryCursor,
) -> Result<bool, Error> {
    let metadata = protocol::Entry {
        name: entry.name,
        name_len: entry.name_len,
        kind: if entry.directory {
            protocol::DIRECTORY
        } else {
            protocol::FILE
        },
        mode: if entry.read_only { 0o555 } else { 0o777 },
        size: entry.size,
        created: entry.created,
        accessed: entry.accessed,
        modified: entry.modified,
    };
    batch.push(&metadata, next).ok_or(Error::Corrupt)
}

impl<D: BlockDevice> FatVolume<D> {
    pub(super) fn fill_entry(
        entry: &fatfs::DirEntry<'_, Disk<D>, crate::fat_time::Clock, fatfs::LossyOemCpConverter>,
        result: &mut Entry,
        readonly: bool,
    ) -> Result<(), Error> {
        result.name_len = 0;
        result.directory = entry.is_dir();
        result.read_only = readonly
            || entry
                .attributes()
                .contains(fatfs::FileAttributes::READ_ONLY);
        result.size = entry.len();
        result.created = crate::fat_time::decode(entry.created());
        result.accessed = crate::fat_time::decode(fatfs::DateTime::new(
            entry.accessed(),
            fatfs::Time::new(0, 0, 0, 0),
        ));
        result.modified = crate::fat_time::decode(entry.modified());
        if let Some(name) = entry.long_file_name_as_ucs2_units() {
            for ch in core::char::decode_utf16(name.iter().copied()) {
                let ch = ch.map_err(|_| Error::Corrupt)?;
                let mut bytes = [0; 4];
                let text = ch.encode_utf8(&mut bytes);
                let end = result
                    .name_len
                    .checked_add(text.len())
                    .filter(|end| *end <= result.name.len())
                    .ok_or(Error::Corrupt)?;
                result.name[result.name_len..end].copy_from_slice(text.as_bytes());
                result.name_len = end;
            }
        } else {
            let name = entry.short_file_name_as_bytes();
            if !name.is_ascii() {
                return Err(Error::Unsupported);
            }
            result.name[..name.len()].copy_from_slice(name);
            result.name_len = name.len();
        }
        if result.name_len == 0 || result.name().contains(['/', '\\', '\0']) {
            return Err(Error::Corrupt);
        }
        Ok(())
    }
}
