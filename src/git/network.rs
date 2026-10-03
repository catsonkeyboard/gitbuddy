//! Cooperative cancellation at libgit2 callbacks and before irreversible steps.
use super::ProgressSink;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU8, Ordering},
};
use std::time::{Duration, Instant};

const RUNNING: u8 = 0;
const CANCEL_REQUESTED: u8 = 1;
const FINISHING: u8 = 2;

#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicU8>);
impl CancellationToken {
    /// Returns false once the operation has entered its final write/upload step.
    pub fn cancel(&self) -> bool {
        self.0
            .compare_exchange(
                RUNNING,
                CANCEL_REQUESTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::Acquire) == CANCEL_REQUESTED
    }
    pub fn can_cancel(&self) -> bool {
        self.0.load(Ordering::Acquire) == RUNNING
    }
    pub fn is_finishing(&self) -> bool {
        self.0.load(Ordering::Acquire) == FINISHING
    }
    pub(super) fn check(&self) -> anyhow::Result<()> {
        if self.is_requested() {
            Err(Cancelled.into())
        } else {
            Ok(())
        }
    }
    pub(super) fn finish(&self) -> anyhow::Result<()> {
        match self
            .0
            .compare_exchange(RUNNING, FINISHING, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) | Err(FINISHING) => Ok(()),
            _ => Err(Cancelled.into()),
        }
    }
}

#[derive(Debug)]
pub struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Operation cancelled")
    }
}
impl std::error::Error for Cancelled {}
pub fn is_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Cancelled>().is_some()
}

#[derive(Clone, Default)]
pub struct NetworkControl {
    pub cancellation: CancellationToken,
    progress: Option<ProgressSink>,
    last_progress: Arc<Mutex<Option<Instant>>>,
}
impl NetworkControl {
    pub fn new(progress: Option<ProgressSink>, cancellation: CancellationToken) -> Self {
        Self {
            progress,
            cancellation,
            last_progress: Arc::default(),
        }
    }
    pub(super) fn check(&self) -> anyhow::Result<()> {
        self.cancellation.check()
    }
    pub(super) fn check_git(&self) -> Result<(), git2::Error> {
        if self.cancellation.is_requested() {
            Err(git2::Error::new(
                git2::ErrorCode::User,
                git2::ErrorClass::Callback,
                "Operation cancelled",
            ))
        } else {
            Ok(())
        }
    }
    pub(super) fn phase(&self, text: &str) {
        if let Some(sink) = &self.progress
            && let Ok(mut sink) = sink.lock()
        {
            sink(text);
        }
    }
    pub(super) fn progress(&self, text: &str) {
        if self.cancellation.is_requested() {
            return;
        }
        let send = if let Ok(mut last) = self.last_progress.lock() {
            if last.is_none_or(|t| t.elapsed() >= Duration::from_millis(100)) {
                *last = Some(Instant::now());
                true
            } else {
                false
            }
        } else {
            false
        };
        if send {
            self.phase(text);
        }
    }
    pub(super) fn normalize<T>(&self, result: anyhow::Result<T>) -> anyhow::Result<T> {
        match result {
            Err(_) if self.cancellation.is_requested() => Err(Cancelled.into()),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_and_finalization_are_mutually_exclusive() {
        let token = CancellationToken::default();
        assert!(token.cancel());
        assert!(is_cancelled(&token.finish().unwrap_err()));
        assert!(!token.can_cancel());
        let token = CancellationToken::default();
        token.finish().unwrap();
        assert!(!token.cancel());
        assert!(!token.is_requested());
        assert!(token.is_finishing());
    }

    #[test]
    fn concurrent_cancel_and_finish_have_exactly_one_winner() {
        for _ in 0..32 {
            let token = CancellationToken::default();
            let other = token.clone();
            let gate = Arc::new(std::sync::Barrier::new(2));
            let worker_gate = gate.clone();
            let worker = std::thread::spawn(move || {
                worker_gate.wait();
                other.cancel()
            });
            gate.wait();
            let finished = token.finish().is_ok();
            assert_ne!(finished, worker.join().unwrap());
            assert_ne!(token.is_requested(), token.is_finishing());
        }
    }
}
