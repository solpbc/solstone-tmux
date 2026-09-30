// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::sync::Mutex;
use std::time::{Duration, Instant};

use time::{OffsetDateTime, UtcOffset};

pub trait Clock: Send + Sync {
    fn wall_now(&self) -> OffsetDateTime;
    fn monotonic_now(&self) -> Duration;
    fn offset_at(&self, instant: OffsetDateTime) -> UtcOffset;
}

#[derive(Clone, Debug)]
pub struct Zone {
    inner: jiff::tz::TimeZone,
}

impl Zone {
    pub fn utc() -> Self {
        Self {
            inner: jiff::tz::TimeZone::UTC,
        }
    }

    pub fn from_tzif(name: &str, data: &[u8]) -> Result<Self, String> {
        jiff::tz::TimeZone::tzif(name, data)
            .map(|inner| Self { inner })
            .map_err(|error| error.to_string())
    }

    pub fn from_posix(posix_tz: &str) -> Result<Self, String> {
        jiff::tz::TimeZone::posix(posix_tz)
            .map(|inner| Self { inner })
            .map_err(|error| error.to_string())
    }

    pub fn iana_name(&self) -> Option<String> {
        self.inner.iana_name().map(str::to_owned)
    }

    pub fn offset_at(&self, instant: OffsetDateTime) -> UtcOffset {
        let timestamp = jiff::Timestamp::from_second(instant.unix_timestamp())
            .expect("instant seconds fit jiff Timestamp range");
        let seconds = self.inner.to_offset(timestamp).seconds();
        UtcOffset::from_whole_seconds(seconds).expect("jiff offset fits time UtcOffset range")
    }

    pub fn fixed(offset: UtcOffset) -> Self {
        let jiff_offset = jiff::tz::Offset::from_seconds(offset.whole_seconds())
            .expect("time UtcOffset seconds fit jiff Offset range");
        Self {
            inner: jiff::tz::TimeZone::fixed(jiff_offset),
        }
    }
}

pub trait ZoneSource: Send {
    fn read(&mut self) -> Result<Zone, String>;
}

pub struct SystemZoneSource;

impl ZoneSource for SystemZoneSource {
    fn read(&mut self) -> Result<Zone, String> {
        resolve_system_zone()
    }
}

pub fn resolve_system_zone() -> Result<Zone, String> {
    jiff::tz::TimeZone::try_system()
        .map(|inner| Zone { inner })
        .map_err(|error| error.to_string())
}

#[derive(Debug)]
pub struct SystemClock {
    monotonic_start: Instant,
    zone: Zone,
}

impl SystemClock {
    pub fn utc() -> Self {
        Self::from_zone(Zone::utc())
    }

    pub fn from_zone(zone: Zone) -> Self {
        Self {
            monotonic_start: Instant::now(),
            zone,
        }
    }

    pub fn iana_name(&self) -> Option<String> {
        self.zone.iana_name()
    }

    pub fn from_resolved(result: Result<Zone, String>) -> (Self, Option<String>) {
        match result {
            Ok(zone) => (Self::from_zone(zone), None),
            Err(cause) => (
                Self::utc(),
                Some(format!(
                    "could not load the local time zone ({cause}); set TZ or repair /etc/localtime"
                )),
            ),
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

    fn offset_at(&self, instant: OffsetDateTime) -> UtcOffset {
        self.zone.offset_at(instant)
    }
}

#[derive(Debug)]
enum TestOffsetRule {
    Fixed(UtcOffset),
    Step {
        before: UtcOffset,
        at: OffsetDateTime,
        at_and_after: UtcOffset,
    },
}

#[derive(Debug)]
pub struct TestClock {
    wall: Mutex<OffsetDateTime>,
    monotonic: Mutex<Duration>,
    offset_rule: TestOffsetRule,
}

impl TestClock {
    pub fn new(wall: OffsetDateTime, monotonic: Duration, local_offset: UtcOffset) -> Self {
        Self {
            wall: Mutex::new(wall),
            monotonic: Mutex::new(monotonic),
            offset_rule: TestOffsetRule::Fixed(local_offset),
        }
    }

    pub fn with_offset_step(
        wall: OffsetDateTime,
        monotonic: Duration,
        before: UtcOffset,
        at: OffsetDateTime,
        at_and_after: UtcOffset,
    ) -> Self {
        Self {
            wall: Mutex::new(wall),
            monotonic: Mutex::new(monotonic),
            offset_rule: TestOffsetRule::Step {
                before,
                at,
                at_and_after,
            },
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

    fn offset_at(&self, instant: OffsetDateTime) -> UtcOffset {
        match self.offset_rule {
            TestOffsetRule::Fixed(offset) => offset,
            TestOffsetRule::Step {
                before,
                at,
                at_and_after,
            } => {
                if instant < at {
                    before
                } else {
                    at_and_after
                }
            }
        }
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
