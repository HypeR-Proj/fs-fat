// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! FAT media parsing and mutation for the Native filesystem service.
#![no_std]
extern crate alloc;
pub use hyper_filesystem::Timestamp;
mod allocation;
pub mod block;
mod fat_time;
mod fat_validate;
mod volume;
pub use volume::{Entry, Error, FatVolume, validate_boot_sector};

#[cfg(test)]
extern crate std;

#[cfg(test)]
#[path = "../tests/volume.rs"]
mod tests;
