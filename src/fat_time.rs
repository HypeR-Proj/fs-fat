// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! FAT dates use UTC on `HypeR`. Validate media values before calendar arithmetic.

use crate::Timestamp;
use fatfs::{Date, DateTime, Time};

fn month_days(year: u16, month: u16) -> Option<u16> {
    Some(match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        _ => return None,
    })
}

pub(super) fn decode(value: DateTime) -> Option<Timestamp> {
    let DateTime { date, time, .. } = value;
    if !(1980..=2107).contains(&date.year)
        || date.day == 0
        || date.day > month_days(date.year, date.month)?
        || time.hour > 23
        || time.min > 59
        || time.sec > 59
        || time.millis > 999
    {
        return None;
    }
    // The on-disk range is small and fixed; this bounded calculation cannot
    // overflow or accept February 31 from a malformed directory entry.
    let mut days = 3652i64; // 1980-01-01 since the Unix epoch.
    for year in 1980..date.year {
        days += if month_days(year, 2)? == 29 { 366 } else { 365 };
    }
    for month in 1..date.month {
        days += i64::from(month_days(date.year, month)?);
    }
    days += i64::from(date.day - 1);
    Timestamp::new(
        days * 86400 + i64::from(time.hour) * 3600 + i64::from(time.min) * 60 + i64::from(time.sec),
        u32::from(time.millis) * 1_000_000,
    )
}

pub(super) fn encode(value: Timestamp) -> Option<DateTime> {
    let mut days = value.seconds().div_euclid(86400).checked_sub(3652)?;
    if !(0..46751).contains(&days) {
        return None;
    }
    let mut year = 1980;
    loop {
        let count = if month_days(year, 2)? == 29 { 366 } else { 365 };
        if days < count {
            break;
        }
        days -= count;
        year += 1;
    }
    if year > 2107 {
        return None;
    }
    let mut month = 1;
    loop {
        let count = i64::from(month_days(year, month)?);
        if days < count {
            break;
        }
        days -= count;
        month += 1;
    }
    let seconds = value.seconds().rem_euclid(86400);
    Some(DateTime::new(
        Date::new(year, month, days as u16 + 1),
        Time::new(
            (seconds / 3600) as u16,
            ((seconds / 60) % 60) as u16,
            (seconds % 60) as u16,
            (value.nanoseconds() / 1_000_000) as u16,
        ),
    ))
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Clock(pub fn() -> Option<Timestamp>);
impl fatfs::TimeProvider for Clock {
    fn get_current_date(&self) -> Date {
        self.get_current_date_time().date
    }
    fn get_current_date_time(&self) -> DateTime {
        (self.0)()
            .and_then(encode)
            .unwrap_or_else(|| DateTime::new(Date::new(1980, 1, 1), Time::new(0, 0, 0, 0)))
    }
}
