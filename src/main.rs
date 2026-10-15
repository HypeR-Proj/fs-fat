// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! FAT media adapter; the common service owns all IPC and mount machinery.

use hyper_filesystem::{
    Timestamp,
    protocol::{self, Entry, Error},
};
use hyper_fs_fat::{
    FatVolume,
    block::{BlockDevice, Error as BlockError},
};
use hyper_fs_service::Volume;
use hyper_os::block::NativeBlock;

struct Device {
    block: NativeBlock,
    sectors: u64,
    readonly: bool,
}

impl BlockDevice for Device {
    fn is_read_only(&self) -> bool {
        self.readonly
    }

    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn read_sectors(&mut self, first: u64, output: &mut [u8]) -> Result<(), BlockError> {
        hyper_fs_fat::block::validate_range(self.sectors, first, output.len())?;
        for (index, bytes) in output.chunks_mut(protocol::DATA_BYTES).enumerate() {
            self.block
                .read_sectors(first + (index * protocol::DATA_BYTES / 512) as u64, bytes)
                .map_err(block_error)?;
        }
        Ok(())
    }

    fn write_sectors(&mut self, first: u64, input: &[u8]) -> Result<(), BlockError> {
        hyper_fs_fat::block::validate_range(self.sectors, first, input.len())?;
        for (index, bytes) in input.chunks(protocol::DATA_BYTES).enumerate() {
            self.block
                .write_sectors(first + (index * protocol::DATA_BYTES / 512) as u64, bytes)
                .map_err(block_error)?;
        }
        Ok(())
    }

    fn write_batch(
        &mut self,
        requests: &[hyper_fs_fat::block::WriteRequest<'_>],
    ) -> Result<(), BlockError> {
        hyper_fs_fat::block::validate_writes(self.sectors, requests)?;
        let mut batch = [hyper_os::block::WriteRequest {
            first: 0,
            bytes: &[],
        }; 4];
        if requests.is_empty() {
            return Ok(());
        }
        if requests.len() > batch.len() {
            return Err(BlockError::InvalidRange);
        }

        for (target, source) in batch.iter_mut().zip(requests) {
            *target = hyper_os::block::WriteRequest {
                first: source.first,
                bytes: source.bytes,
            };
        }

        self.block
            .write_batch(&batch[..requests.len()])
            .map_err(block_error)
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        self.block.flush().map_err(block_error)
    }
}

struct Fat(FatVolume<Device>);

impl Volume for Fat {
    fn stat(&mut self, path: &str) -> Result<Entry, Error> {
        self.0.stat(path).map(metadata).map_err(map_error)
    }

    fn read_directory(
        &mut self,
        path: &str,
        cursor: protocol::DirectoryCursor,
        output: &mut [u8],
    ) -> Result<usize, Error> {
        self.0
            .read_directory(path, cursor, output)
            .map_err(map_error)
    }

    fn read(&mut self, path: &str, offset: u64, output: &mut [u8]) -> Result<usize, Error> {
        self.0.read_at(path, offset, output).map_err(map_error)
    }

    fn write(&mut self, path: &str, offset: u64, input: &[u8]) -> Result<usize, Error> {
        self.0.write_at(path, offset, input).map_err(map_error)
    }

    fn create(&mut self, path: &str, directory: bool) -> Result<(), Error> {
        self.0.create(path, directory).map_err(map_error)
    }

    fn remove(&mut self, path: &str) -> Result<(), Error> {
        self.0.remove(path).map_err(map_error)
    }

    fn rename(&mut self, path: &str, target: &str) -> Result<(), Error> {
        self.0.rename(path, target).map_err(map_error)
    }

    fn resize(&mut self, path: &str, length: u64) -> Result<(), Error> {
        self.0.resize(path, length).map_err(map_error)
    }

    fn sync(&mut self) -> Result<(), Error> {
        self.0.sync().map_err(map_error)
    }

    fn set_times(
        &mut self,
        path: &str,
        a: Option<Timestamp>,
        m: Option<Timestamp>,
    ) -> Result<(), Error> {
        self.0.set_times(path, a, m).map_err(map_error)
    }
}

fn metadata(value: hyper_fs_fat::Entry) -> Entry {
    Entry {
        name: value.name,
        name_len: value.name_len,
        kind: if value.directory {
            protocol::DIRECTORY
        } else {
            protocol::FILE
        },
        mode: if value.read_only { 0o555 } else { 0o777 },
        size: value.size,
        created: value.created,
        accessed: value.accessed,
        modified: value.modified,
    }
}

fn block_error(error: hyper_os::Error) -> BlockError {
    match error {
        hyper_os::Error::Status(hyper_os::Status::READ_ONLY) => BlockError::ReadOnly,
        hyper_os::Error::Status(hyper_os::Status::INVALID_ARGUMENT) => BlockError::InvalidRange,
        _ => BlockError::Io,
    }
}

fn map_error(error: hyper_fs_fat::Error) -> Error {
    use hyper_fs_fat::Error as E;

    match error {
        E::Allocation => Error::Allocation,
        E::Corrupt => Error::Corrupt,
        E::Missing => Error::Missing,
        E::Exists => Error::Exists,
        E::NotEmpty => Error::NotEmpty,
        E::InvalidInput => Error::InvalidInput,
        E::NoSpace => Error::NoSpace,
        E::Unsupported => Error::Unsupported,
        E::Closed => Error::Closed,
        E::Block(error) => match error {
            BlockError::ReadOnly => Error::ReadOnly,
            BlockError::InvalidRange => Error::InvalidInput,
            BlockError::Disconnected => Error::Closed,
            BlockError::Corrupt => Error::Corrupt,
            BlockError::Exhausted => Error::ResourceLimit,
            BlockError::Unsupported => Error::Unsupported,
            BlockError::Io => Error::Io,
        },
    }
}

fn realtime() -> Option<Timestamp> {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Timestamp::new(
        i64::try_from(duration.as_secs()).ok()?,
        duration.subsec_nanos(),
    )
}

fn run() -> Result<(), String> {
    let mut startup = hyper_rt::process::startup().map_err(show)?;
    let sectors = std::env::args()
        .nth(1)
        .ok_or("missing volume geometry")?
        .parse::<u64>()
        .map_err(show)?;
    let readonly = match std::env::args().nth(2).as_deref() {
        Some("ro") => true,
        Some("rw") => false,
        _ => return Err("invalid volume access mode".into()),
    };

    let (block, adopted) = if std::env::args().nth(3).as_deref() == Some("--adopt") {
        let (block, state) = hyper_fs_service::adopt(&mut startup)?;
        (block, Some(state))
    } else {
        (
            NativeBlock::from_handle(
                startup
                    .take(hyper_service::filesystem::BLOCK)
                    .map_err(show)?,
            ),
            None,
        )
    };

    let volume = FatVolume::mount_with_clock(
        Device {
            block,
            sectors,
            readonly,
        },
        realtime,
    )
    .map_err(show)?;

    let mut volume = Fat(volume);
    let completion = match adopted {
        Some(state) => hyper_fs_service::resume(state, &mut volume)?,
        None => hyper_fs_service::serve(&mut startup, &mut volume)?,
    };
    let device = volume.0.into_device().map_err(show)?;
    hyper_fs_service::return_block(completion, device.block)
}

fn show(error: impl core::fmt::Debug) -> String {
    format!("{error:?}")
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("HypeR fs-fat: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
