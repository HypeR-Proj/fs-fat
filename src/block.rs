// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Synchronous, sleepable access to an exclusively owned logical block volume.

pub const SECTOR_SIZE: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidRange,
    ReadOnly,
    Disconnected,
    Io,
    Unsupported,
    Exhausted,
    Corrupt,
}

/// Complete sectors starting at a volume-relative sector address.
#[derive(Clone, Copy)]
pub struct WriteRequest<'a> {
    pub first: u64,
    pub bytes: &'a [u8],
}

/// A volume, not a physical disk or an implicitly selected partition.
///
/// Calls may sleep. Owners must serialize access with a sleepable lock, never
/// an IRQ mask or spin lock. Successful transfers complete the entire slice;
/// an error may have partially modified the medium. `flush` must wait for all
/// earlier writes to become durable, or return an error. Dropping a device
/// must not perform I/O. Geometry remains fixed until the device is retired.
/// This revision admits 512-byte logical sectors only.
pub trait BlockDevice {
    fn sector_count(&self) -> u64;
    /// Immutable admission policy for this exclusive device lifetime.
    fn is_read_only(&self) -> bool {
        false
    }
    fn read_sectors(&mut self, first: u64, output: &mut [u8]) -> Result<(), Error>;
    fn write_sectors(&mut self, first: u64, input: &[u8]) -> Result<(), Error>;
    /// Complete disjoint writes, possibly in parallel and in any order. Check
    /// every range before submitting the first request. On error, any subset
    /// may have reached the medium; no borrowed input outlives this call.
    fn write_batch(&mut self, requests: &[WriteRequest<'_>]) -> Result<(), Error> {
        validate_writes(self.sector_count(), requests)?;
        for request in requests {
            if !request.bytes.is_empty() {
                self.write_sectors(request.first, request.bytes)?;
            }
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<(), Error>;
}

pub fn validate_writes(sector_count: u64, requests: &[WriteRequest<'_>]) -> Result<(), Error> {
    for (index, request) in requests.iter().enumerate() {
        validate_range(sector_count, request.first, request.bytes.len())?;
        if request.bytes.is_empty() {
            continue;
        }
        let end = request.first + (request.bytes.len() / SECTOR_SIZE) as u64;
        for previous in &requests[..index] {
            let previous_end = previous.first + (previous.bytes.len() / SECTOR_SIZE) as u64;
            if !previous.bytes.is_empty() && request.first < previous_end && previous.first < end {
                return Err(Error::InvalidRange);
            }
        }
    }
    Ok(())
}

/// Validate before issuing any transfer, including a zero-length request.
pub fn validate_range(sector_count: u64, first: u64, bytes: usize) -> Result<(), Error> {
    if !bytes.is_multiple_of(SECTOR_SIZE) {
        return Err(Error::InvalidRange);
    }
    let sectors = u64::try_from(bytes / SECTOR_SIZE).map_err(|_| Error::InvalidRange)?;
    if first
        .checked_add(sectors)
        .is_none_or(|end| end > sector_count)
    {
        return Err(Error::InvalidRange);
    }
    Ok(())
}
