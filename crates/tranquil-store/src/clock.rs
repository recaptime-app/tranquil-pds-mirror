use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::blockstore::WallClockMs;
use crate::eventlog::TimestampMicros;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct LogicalNanos(u64);

impl LogicalNanos {
    pub const fn new(nanos: u64) -> Self {
        Self(nanos)
    }

    pub fn raw(self) -> u64 {
        self.0
    }

    pub fn from_millis(ms: u64) -> Self {
        Self(ms.saturating_mul(1_000_000))
    }

    pub fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

pub trait Clock: Clone + Send + Sync + 'static {
    fn unix_micros(&self) -> TimestampMicros;
    fn wall_millis(&self) -> WallClockMs;
    fn monotonic(&self) -> LogicalNanos;
    fn advance(&self, by: Duration);
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

static PROCESS_ORIGIN: OnceLock<Instant> = OnceLock::new();

impl Clock for SystemClock {
    fn unix_micros(&self) -> TimestampMicros {
        TimestampMicros::now()
    }

    fn wall_millis(&self) -> WallClockMs {
        WallClockMs::now()
    }

    fn monotonic(&self) -> LogicalNanos {
        let origin = PROCESS_ORIGIN.get_or_init(Instant::now);
        LogicalNanos::new(u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX))
    }

    fn advance(&self, _by: Duration) {}
}

#[cfg(any(test, feature = "test-harness"))]
pub use sim_clock::SimClock;

#[cfg(any(test, feature = "test-harness"))]
mod sim_clock {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::{Clock, LogicalNanos};
    use crate::blockstore::WallClockMs;
    use crate::eventlog::TimestampMicros;
    use crate::sim::splitmix64;

    const EPOCH_BASE_MICROS: u64 = 1_700_000_000_000_000;
    const ORIGIN_SPREAD_MICROS: u64 = 86_400 * 1_000_000;

    #[derive(Debug)]
    struct SimClockState {
        logical_nanos: AtomicU64,
        unix_origin_micros: u64,
    }

    #[derive(Debug, Clone)]
    pub struct SimClock {
        state: Arc<SimClockState>,
    }

    impl SimClock {
        pub fn new(seed: u64) -> Self {
            Self {
                state: Arc::new(SimClockState {
                    logical_nanos: AtomicU64::new(0),
                    unix_origin_micros: EPOCH_BASE_MICROS
                        + (splitmix64(seed) % ORIGIN_SPREAD_MICROS),
                }),
            }
        }
    }

    impl Clock for SimClock {
        fn unix_micros(&self) -> TimestampMicros {
            let logical = self.state.logical_nanos.load(Ordering::Acquire);
            TimestampMicros::new(
                self.state
                    .unix_origin_micros
                    .saturating_add(logical / 1_000),
            )
        }

        fn wall_millis(&self) -> WallClockMs {
            let logical = self.state.logical_nanos.load(Ordering::Acquire);
            WallClockMs::new(
                (self.state.unix_origin_micros / 1_000).saturating_add(logical / 1_000_000),
            )
        }

        fn monotonic(&self) -> LogicalNanos {
            LogicalNanos::new(self.state.logical_nanos.load(Ordering::Acquire))
        }

        fn advance(&self, by: Duration) {
            let nanos = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
            self.state.logical_nanos.fetch_add(nanos, Ordering::AcqRel);
        }
    }
}
