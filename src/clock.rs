//! The clock the user interface reads, instead of the system's.
//!
//! Two things on screen come from the clock rather than from mail: the date
//! column of the message list, which drops the year for anything sent this
//! year, and the throbbers that count the seconds a background operation has
//! been running.  Both are why the same application state renders differently
//! at different moments, which is precisely what a snapshot test -- or a demo
//! recording that should look the same next year -- cannot have.
//!
//! [`freeze`] holds the clock still for the calling thread.  It exists only in
//! builds that can want it, the tests and the `recorder` feature; everywhere
//! else [`frozen`] is a function returning `None` and the branch folds away.

use std::time::{Duration, Instant};
use time::OffsetDateTime;

/// What the interface calls "now".
pub(crate) fn now_utc() -> OffsetDateTime {
    frozen().map_or_else(OffsetDateTime::now_utc, |(now, _)| now)
}

/// How long ago `since` was.
pub(crate) fn elapsed(since: Instant) -> Duration {
    frozen().map_or_else(|| since.elapsed(), |(_, elapsed)| elapsed)
}

/// Nothing is ever frozen in a normal build.
#[cfg(not(any(test, feature = "recorder")))]
fn frozen() -> Option<(OffsetDateTime, Duration)> {
    None
}

#[cfg(any(test, feature = "recorder"))]
thread_local! {
    /// Wall-clock reading and operation age this thread reports, if frozen.
    static FROZEN: std::cell::Cell<Option<(OffsetDateTime, Duration)>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(any(test, feature = "recorder"))]
fn frozen() -> Option<(OffsetDateTime, Duration)> {
    FROZEN.with(|frozen| frozen.get())
}

/// Hold time still on this thread: [`now_utc`] answers `now`, and every
/// operation reports `elapsed` as its age however long it has really been
/// running.
///
/// A single age for everything is coarse, but the age is only ever rendered as
/// a throbber frame and a tenth-of-a-second counter, and a caller that wants to
/// see a different one just freezes the clock again before drawing.  Per thread
/// rather than per process, so tests running next to each other -- `cargo test`
/// gives each its own thread -- cannot disturb one another.
///
/// Unused in a `recorder` build of the binary: what calls it there is the demo
/// example, which compiles this module into itself.
#[cfg(any(test, feature = "recorder"))]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn freeze(now: OffsetDateTime, elapsed: Duration) {
    FROZEN.with(|frozen| frozen.set(Some((now, elapsed))));
}
