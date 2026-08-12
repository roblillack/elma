//! The clock the user interface reads, instead of the system's.
//!
//! Two things on screen come from the clock rather than from mail: the date
//! column of the message list, which drops the year for anything sent this
//! year, and the throbbers that count the seconds a background operation has
//! been running.  Both are why the same application state renders differently
//! at different moments, which is precisely what a snapshot test cannot have.
//!
//! Outside of tests these are the calls they stand in for.  Under `cfg(test)`
//! they consult a per-thread override that [`freeze`] installs, so a test can
//! hold time still for the app it drives without affecting anything running
//! next to it -- `cargo test` runs each test on its own thread.

use std::time::{Duration, Instant};
use time::OffsetDateTime;

#[cfg(not(test))]
pub(crate) fn now_utc() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// How long ago `since` was.
#[cfg(not(test))]
pub(crate) fn elapsed(since: Instant) -> Duration {
    since.elapsed()
}

#[cfg(test)]
thread_local! {
    /// Wall-clock reading and operation age this thread reports, if frozen.
    static FROZEN: std::cell::Cell<Option<(OffsetDateTime, Duration)>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn now_utc() -> OffsetDateTime {
    FROZEN
        .with(|frozen| frozen.get())
        .map(|(now, _)| now)
        .unwrap_or_else(OffsetDateTime::now_utc)
}

#[cfg(test)]
pub(crate) fn elapsed(since: Instant) -> Duration {
    FROZEN
        .with(|frozen| frozen.get())
        .map(|(_, elapsed)| elapsed)
        .unwrap_or_else(|| since.elapsed())
}

/// Hold time still on this thread: [`now_utc`] answers `now`, and every
/// operation reports `elapsed` as its age however long it has really been
/// running.
///
/// A single age for everything is coarse, but the age is only ever rendered as
/// a throbber frame and a tenth-of-a-second counter, and a test that wants to
/// see a different one just freezes the clock again before drawing.
#[cfg(test)]
pub(crate) fn freeze(now: OffsetDateTime, elapsed: Duration) {
    FROZEN.with(|frozen| frozen.set(Some((now, elapsed))));
}
