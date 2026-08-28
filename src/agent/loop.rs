use crate::agent::state::TurnLimits;
use std::time::{Duration, Instant};

pub(crate) fn deadline(limits: TurnLimits) -> Instant {
    Instant::now() + Duration::from_secs(limits.elapsed_seconds)
}

pub(crate) fn within_budget(iteration: usize, limits: TurnLimits, deadline: Instant) -> bool {
    iteration < limits.tool_iterations && Instant::now() < deadline
}
