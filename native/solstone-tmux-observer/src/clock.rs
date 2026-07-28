// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::sync::Mutex;
use std::time::{Duration, Instant};

use time::{OffsetDateTime, UtcOffset};

pub trait Clock: Send + Sync {
    fn wall_now(&self) -> OffsetDateTime;
    fn monotonic_now(&self) -> Duration;
    fn local_offset(&self) -> UtcOffset;
}

#[derive(Debug)]
pub struct SystemClock {
    monotonic_start: Instant,
    local_offset: UtcOffset,
}

impl SystemClock {
    pub fn new(local_offset: UtcOffset) -> Self {
        Self {
            monotonic_start: Instant::now(),
            local_offset,
        }
    }
}

impl Clock for SystemClock {
    fn wall_now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn monotonic_now(&self) -> Duration {
        self.monotonic_start.elapsed()
    }

    fn local_offset(&self) -> UtcOffset {
        self.local_offset
    }
}

#[derive(Debug)]
pub struct TestClock {
    wall: Mutex<OffsetDateTime>,
    monotonic: Mutex<Duration>,
    local_offset: UtcOffset,
}

impl TestClock {
    pub fn new(wall: OffsetDateTime, monotonic: Duration, local_offset: UtcOffset) -> Self {
        Self {
            wall: Mutex::new(wall),
            monotonic: Mutex::new(monotonic),
            local_offset,
        }
    }

    pub fn set_wall(&self, wall: OffsetDateTime) {
        *self.wall.lock().expect("test wall clock poisoned") = wall;
    }

    pub fn set_monotonic(&self, monotonic: Duration) {
        *self
            .monotonic
            .lock()
            .expect("test monotonic clock poisoned") = monotonic;
    }
}

impl Clock for TestClock {
    fn wall_now(&self) -> OffsetDateTime {
        *self.wall.lock().expect("test wall clock poisoned")
    }

    fn monotonic_now(&self) -> Duration {
        *self
            .monotonic
            .lock()
            .expect("test monotonic clock poisoned")
    }

    fn local_offset(&self) -> UtcOffset {
        self.local_offset
    }
}

pub fn local_date_and_time(wall_now: OffsetDateTime, local_offset: UtcOffset) -> (String, String) {
    let local = wall_now.to_offset(local_offset);
    (
        format!(
            "{:04}{:02}{:02}",
            local.year(),
            u8::from(local.month()),
            local.day()
        ),
        format!(
            "{:02}{:02}{:02}",
            local.hour(),
            local.minute(),
            local.second()
        ),
    )
}
