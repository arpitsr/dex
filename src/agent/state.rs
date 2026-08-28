#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TurnLimits {
    pub elapsed_seconds: u64,
}

pub(crate) trait CancellationSource {
    fn is_cancelled(&self) -> bool;
    fn take_cancelled(&self) -> bool;
}

pub(crate) struct GlobalCancellation;

impl CancellationSource for GlobalCancellation {
    fn is_cancelled(&self) -> bool {
        crate::cancel_requested()
    }
    fn take_cancelled(&self) -> bool {
        crate::take_cancel_requested()
    }
}
