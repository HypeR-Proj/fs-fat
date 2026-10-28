// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Sleepable block access with operation-scoped asynchronous write submission.

pub const SECTOR_SIZE: usize = 512;
/// Maximum descriptors in one admission. Larger drains stream several batches.
pub const WRITE_BATCH_REQUESTS: usize = 16;
/// Maximum payload in one atomic queue admission.
pub const WRITE_BATCH_BYTES: usize = 512 * 1024;

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
    /// Establish a readonly kernel-window input for this synchronous filesystem
    /// operation. Generic devices retain the ordinary copying implementation.
    fn begin_write_window(&mut self, _token: u64, _input: &[u8]) -> Result<(), Error> {
        Ok(())
    }
    fn end_write_window(&mut self) {}
    /// This is an optimization hint only; the kernel validates the operation
    /// token and every submitted offset independently of this pointer check.
    fn is_write_window(&self, _input: &[u8]) -> bool {
        false
    }
    fn submit_write_window(&mut self, _first: u64, _input: &[u8]) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
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
    /// Accept disjoint writes without requiring device completion. Implementors
    /// must copy the input before returning and bound outstanding resources.
    /// Later overlapping reads/writes observe submission order; independent
    /// requests may complete in any order. Backpressure may sleep to reap prior
    /// submissions. A failed admission can leave earlier batches outstanding.
    fn submit_writes(&mut self, requests: &[WriteRequest<'_>]) -> Result<(), Error> {
        self.write_batch(requests)
    }
    /// Join every accepted write, including after another operation failed.
    /// This is a completion boundary, not a physical durability barrier. Return
    /// the first failure only after retiring all outstanding completion tokens.
    fn complete_writes(&mut self) -> Result<(), Error> {
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
