//! Off-UI-thread execution. Every disk/CPU-heavy operation runs on a dedicated OS thread and
//! reports back through channels, so the render loop never blocks.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use iced::Task;
use iced::futures::channel::{mpsc, oneshot};
use psx2psp_engine::core::Monitor;

/// Runs `f` on a fresh thread; the returned task resolves with its result.
pub fn blocking<T, M>(f: impl FnOnce() -> Result<T, String> + Send + 'static, done: impl Fn(Result<T, String>) -> M + Send + 'static) -> Task<M>
where
    T: Send + 'static,
    M: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    Task::perform(async move { rx.await.unwrap_or_else(|_| Err("worker thread crashed".into())) }, done)
}

#[derive(Debug, Clone)]
pub enum JobEvent {
    /// 0.0..=1.0
    Progress(f32),
    Log(String),
    Done(Result<String, String>),
}

struct ChannelMonitor {
    tx: mpsc::UnboundedSender<JobEvent>,
    cancel: Arc<AtomicBool>,
    /// Last reported progress in 1/1000 steps: throttles UI messages to ≤1000 per job.
    last: AtomicU32,
}

impl Monitor for ChannelMonitor {
    fn progress(&self, done: u64, total: u64) {
        let permille = (done.saturating_mul(1000) / total.max(1)).min(1000) as u32;
        if self.last.swap(permille, Ordering::Relaxed) != permille {
            let _ = self.tx.unbounded_send(JobEvent::Progress(permille as f32 / 1000.0));
        }
    }
    fn log(&self, m: &str) {
        let _ = self.tx.unbounded_send(JobEvent::Log(m.to_owned()));
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Long-running cancellable job streaming progress/log events and a final `Done`.
pub fn job<M: Send + 'static>(
    cancel: Arc<AtomicBool>,
    f: impl FnOnce(&dyn Monitor) -> Result<String, String> + Send + 'static,
    map: impl Fn(JobEvent) -> M + Send + 'static,
) -> Task<M> {
    let (tx, rx) = mpsc::unbounded();
    std::thread::spawn(move || {
        let monitor = ChannelMonitor { tx, cancel, last: AtomicU32::new(u32::MAX) };
        // A panicking job must still release the UI from its "running" state (debug builds unwind).
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&monitor)))
            .unwrap_or_else(|_| Err("internal error: worker panicked".into()));
        let _ = monitor.tx.unbounded_send(JobEvent::Done(result));
    });
    Task::stream(rx).map(map)
}
