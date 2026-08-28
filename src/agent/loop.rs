use crate::agent::state::TurnLimits;
use std::time::{Duration, Instant};

pub(crate) fn deadline(limits: TurnLimits) -> Instant {
    Instant::now() + Duration::from_secs(limits.elapsed_seconds)
}

pub(crate) fn within_budget(deadline: Instant) -> bool {
    Instant::now() < deadline
}
