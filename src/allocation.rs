// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use alloc::{alloc::alloc, boxed::Box};
use core::{alloc::Layout, ptr::NonNull};
pub(crate) fn try_box<T>(value: T) -> Result<Box<T>, ()> {
    let layout = Layout::new::<T>();
    if layout.size() == 0 {
        return Ok(Box::new(value));
    }
    // SAFETY: Nonzero layout is the exact Box<T> allocation layout.
    let pointer = NonNull::new(unsafe { alloc(layout) }.cast::<T>()).ok_or(())?;
    // SAFETY: Fresh aligned allocation is uniquely owned and initialized once.
    unsafe {
        pointer.as_ptr().write(value);
        Ok(Box::from_raw(pointer.as_ptr()))
    }
}
