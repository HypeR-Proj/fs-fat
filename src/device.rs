// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! One logical queue owns the FAT operation's outstanding block writes. The
//! kernel owns submitted data and orders overlapping ranges. File payloads use
//! scoped readonly windows; metadata uses copy-on-submit. Neither userspace
//! source is directly visible to device DMA.

use hyper_fs_fat::block::{self, BlockDevice, Error, WriteRequest};
use hyper_os::block::{BlockQueue, NativeBlock, Ticket, WindowWrite};

const REQUESTS: usize = 16;
const TRANSFER_BYTES: usize = block::WRITE_BATCH_BYTES;

pub(super) struct Device {
    block: NativeBlock,
    queue: Option<BlockQueue>,
    pending: [Option<Ticket>; REQUESTS],
    sectors: u64,
    readonly: bool,
    window: Option<Source>,
    ranges: [WindowWrite; block::WRITE_BATCH_REQUESTS],
    range_count: usize,
    range_bytes: usize,
}

#[derive(Clone, Copy)]
struct Source {
    token: u64,
    address: usize,
    length: usize,
}

impl Source {
    fn offset(self, input: &[u8]) -> Option<usize> {
        let offset = (input.as_ptr() as usize).checked_sub(self.address)?;
        (offset.checked_add(input.len())? <= self.length).then_some(offset)
    }
}

impl Device {
    pub(super) fn new(
        block: NativeBlock,
        sectors: u64,
        readonly: bool,
        queue: BlockQueue,
    ) -> Result<Self, Error> {
        Ok(Self {
            block,
            queue: Some(queue),
            pending: [None; REQUESTS],
            sectors,
            readonly,
            window: None,
            ranges: [WindowWrite {
                first: 0,
                offset: 0,
                bytes: 0,
            }; block::WRITE_BATCH_REQUESTS],
            range_count: 0,
            range_bytes: 0,
        })
    }

    /// End the derived queue before handing the original volume capability to
    /// another worker. No accepted write may cross that ownership boundary.
    pub(super) fn into_block(mut self) -> Result<NativeBlock, Error> {
        self.complete_writes()?;
        drop(self.queue.take());
        Ok(self.block)
    }

    fn queue(&self) -> Result<&BlockQueue, Error> {
        self.queue.as_ref().ok_or(Error::Disconnected)
    }

    fn submit_window_ranges(&mut self) -> Result<(), Error> {
        if self.range_count == 0 {
            return Ok(());
        }
        let token = self.window.ok_or(Error::InvalidRange)?.token;
        if self.pending.iter().all(Option::is_some) {
            self.complete_one()?;
        }
        let index = self
            .pending
            .iter()
            .position(Option::is_none)
            .ok_or(Error::Exhausted)?;
        let count = self.range_count;
        let ranges = self.ranges;
        let payload = self.range_bytes;
        // Clear before admission: an error is terminal for this volume, and
        // completion cleanup must never resubmit a possibly accepted range.
        self.range_count = 0;
        self.range_bytes = 0;
        let ticket = self.admit(payload, |queue| {
            queue.submit_window(token, &ranges[..count])
        })?;
        self.pending[index] = Some(ticket);
        Ok(())
    }

    fn abandon(&mut self, error: hyper_os::Error) -> Error {
        // A failed wait/reap syscall cannot prove physical completion. Relinquish
        // this consumer; the transport may retain another queue capability.
        // Kernel request leases retain backing and prevent window reuse until
        // outstanding submissions finish, even after this worker fails.
        drop(self.queue.take());
        self.pending.fill(None);
        self.range_count = 0;
        self.range_bytes = 0;
        block_error(error)
    }

    fn admit(
        &mut self,
        bytes: usize,
        submit: impl Fn(&BlockQueue) -> hyper_os::Result<Ticket>,
    ) -> Result<Ticket, Error> {
        loop {
            match submit(self.queue()?) {
                Ok(ticket) => return Ok(ticket),
                Err(hyper_os::Error::Status(hyper_os::Status::WOULD_BLOCK)) => {
                    // Return our own completed credits first. With no local
                    // tickets, contention belongs to another logical queue:
                    // wait for this request's exact device/queue space instead
                    // of poisoning the filesystem for temporary backpressure.
                    if self.pending.iter().all(Option::is_none) {
                        if let Err(error) = self.queue()?.wait_space(bytes, u64::MAX) {
                            return Err(self.abandon(error));
                        }
                    } else {
                        self.complete_one()?;
                    }
                }
                Err(error) => return Err(block_error(error)),
            }
        }
    }

    fn complete_one(&mut self) -> Result<(), Error> {
        let completion = match self.queue()?.wait_any(u64::MAX) {
            Ok(completion) => completion,
            Err(error) => return Err(self.abandon(error)),
        };
        let Some(index) = self
            .pending
            .iter()
            .position(|ticket| *ticket == Some(completion.ticket))
        else {
            return Err(self.abandon(hyper_os::Error::InvalidResponse));
        };
        let completion = match self.queue()?.reap(completion.ticket, &mut []) {
            Ok(completion) => completion,
            Err(error) => return Err(self.abandon(error)),
        };
        self.pending[index] = None;
        completion.result().map_err(block_error)
    }

    fn finish(&mut self, ticket: Ticket, output: &mut [u8]) -> Result<(), Error> {
        let expected = output.len();
        let completion = match self.queue()?.wait(ticket, u64::MAX) {
            Ok(completion) => completion,
            Err(error) => return Err(self.abandon(error)),
        };
        let bytes = if completion.status == hyper_os::Status::OK {
            output
        } else {
            &mut []
        };
        let completion = match self.queue()?.reap(ticket, bytes) {
            Ok(completion) => completion,
            Err(error) => return Err(self.abandon(error)),
        };
        completion.result().map_err(block_error)?;
        if completion.bytes != expected {
            return Err(self.abandon(hyper_os::Error::InvalidResponse));
        }
        Ok(())
    }
}

impl BlockDevice for Device {
    fn begin_write_window(&mut self, token: u64, input: &[u8]) -> Result<(), Error> {
        if token == 0
            || self.window.is_some()
            || self.pending.iter().any(Option::is_some)
            || self.range_count != 0
        {
            return Err(Error::InvalidRange);
        }
        self.window = Some(Source {
            token,
            address: input.as_ptr() as usize,
            length: input.len(),
        });
        Ok(())
    }

    fn end_write_window(&mut self) {
        self.window = None;
    }

    fn is_write_window(&self, input: &[u8]) -> bool {
        self.window
            .is_some_and(|source| source.offset(input).is_some())
    }

    fn submit_write_window(&mut self, first: u64, input: &[u8]) -> Result<(), Error> {
        block::validate_range(self.sectors, first, input.len())?;
        if self.readonly {
            return Err(Error::ReadOnly);
        }
        let source = self.window.ok_or(Error::InvalidRange)?;
        let offset = source.offset(input).ok_or(Error::InvalidRange)?;
        if input.len() > TRANSFER_BYTES {
            return Err(Error::InvalidRange);
        }
        if self.range_count != 0 && self.range_bytes + input.len() > TRANSFER_BYTES {
            self.submit_window_ranges()?;
        }
        let range = WindowWrite {
            first,
            offset: offset as u64,
            bytes: input.len() as u64,
        };
        let previous = self
            .range_count
            .checked_sub(1)
            .and_then(|index| self.ranges.get_mut(index));
        if let Some(previous) = previous.filter(|previous| {
            previous.first + previous.bytes / block::SECTOR_SIZE as u64 == first
                && previous.offset + previous.bytes == offset as u64
        }) {
            previous.bytes += range.bytes;
        } else {
            if self.range_count == self.ranges.len() {
                self.submit_window_ranges()?;
            }
            self.ranges[self.range_count] = range;
            self.range_count += 1;
        }
        self.range_bytes += input.len();
        if self.range_bytes >= 64 * 1024 {
            self.submit_window_ranges()?;
        }
        Ok(())
    }

    fn is_read_only(&self) -> bool {
        self.readonly
    }

    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn read_sectors(&mut self, first: u64, output: &mut [u8]) -> Result<(), Error> {
        self.submit_window_ranges()?;
        block::validate_range(self.sectors, first, output.len())?;
        for (index, bytes) in output.chunks_mut(TRANSFER_BYTES).enumerate() {
            let sector = first + (index * TRANSFER_BYTES / block::SECTOR_SIZE) as u64;
            let ticket = self.admit(bytes.len(), |queue| queue.submit_read(sector, bytes.len()))?;
            self.finish(ticket, bytes)?;
        }
        Ok(())
    }

    fn write_sectors(&mut self, first: u64, input: &[u8]) -> Result<(), Error> {
        block::validate_range(self.sectors, first, input.len())?;
        let submitted = (|| {
            for (index, bytes) in input.chunks(TRANSFER_BYTES).enumerate() {
                let sector = first + (index * TRANSFER_BYTES / block::SECTOR_SIZE) as u64;
                self.submit_writes(&[WriteRequest {
                    first: sector,
                    bytes,
                }])?;
            }
            Ok(())
        })();
        let completed = self.complete_writes();
        submitted.and(completed)
    }

    fn submit_writes(&mut self, requests: &[WriteRequest<'_>]) -> Result<(), Error> {
        self.submit_window_ranges()?;
        block::validate_writes(self.sectors, requests)?;
        if requests.is_empty() {
            return Ok(());
        }
        if self.readonly {
            return Err(Error::ReadOnly);
        }
        if requests.len() > block::WRITE_BATCH_REQUESTS {
            return Err(Error::InvalidRange);
        }
        let payload = requests.iter().try_fold(0usize, |length, request| {
            length
                .checked_add(request.bytes.len())
                .ok_or(Error::InvalidRange)
        })?;
        if payload > block::WRITE_BATCH_BYTES {
            return Err(Error::InvalidRange);
        }
        let mut batch = [hyper_os::block::WriteRequest {
            first: 0,
            bytes: &[],
        }; block::WRITE_BATCH_REQUESTS];
        for (target, source) in batch.iter_mut().zip(requests) {
            *target = hyper_os::block::WriteRequest {
                first: source.first,
                bytes: source.bytes,
            };
        }
        if self.pending.iter().all(Option::is_some) {
            self.complete_one()?;
        }
        let index = self
            .pending
            .iter()
            .position(Option::is_none)
            .ok_or(Error::Exhausted)?;
        let ticket = self.admit(payload, |queue| {
            queue.submit_writes(&batch[..requests.len()])
        })?;
        self.pending[index] = Some(ticket);
        Ok(())
    }

    fn complete_writes(&mut self) -> Result<(), Error> {
        let mut failure = self.submit_window_ranges().err();
        while self.pending.iter().any(Option::is_some) {
            if let Err(error) = self.complete_one() {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.complete_writes()?;
        let ticket = self.admit(0, BlockQueue::submit_flush)?;
        self.finish(ticket, &mut [])
    }
}

fn block_error(error: hyper_os::Error) -> Error {
    match error {
        hyper_os::Error::Status(hyper_os::Status::READ_ONLY) => Error::ReadOnly,
        hyper_os::Error::Status(hyper_os::Status::PEER_CLOSED) => Error::Disconnected,
        hyper_os::Error::Status(hyper_os::Status::INVALID_ARGUMENT) => Error::InvalidRange,
        hyper_os::Error::Status(hyper_os::Status::RESOURCE_LIMIT | hyper_os::Status::NO_MEMORY) => {
            Error::Exhausted
        }
        _ => Error::Io,
    }
}
