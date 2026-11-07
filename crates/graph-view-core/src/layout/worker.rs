//! A std-thread driver for one layout budget.
//!
//! [`GraphScene::step_layout_async`](crate::scene::GraphScene::step_layout_async) uses GPUI's
//! background executor, which a host without GPUI cannot drive. A host that runs the renderer
//! elsewhere (for example a Rune or egui console) still needs layout off its frame thread, so this
//! module runs one [`LayoutEngine::step`] on a dedicated OS thread and hands the engine back.
//!
//! The thread owns nothing but the step: which engine it runs, when a result is adopted, and what
//! makes a result stale are [`GraphScene`](crate::scene::GraphScene)'s decisions, shared with the
//! GPUI driver through one adoption rule.

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};

use super::CompletedLayoutStep;
use super::session::LayoutFlight;

/// A layout step running on a dedicated OS thread.
pub(crate) struct ThreadFlight {
    // `Receiver` is `Send` but not `Sync`, and the scene holding this flight
    // must stay `Sync` (the paint layer captures it). The mutex only guards
    // the poll; the thread sends without touching it.
    finished: Mutex<Receiver<CompletedLayoutStep>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ThreadFlight {
    /// The finished step, or the flight itself when the thread is still
    /// running. A thread that exits without a result (it panicked) is a bug in
    /// the engine, not a recoverable state.
    pub(crate) fn try_finish(mut self) -> Result<CompletedLayoutStep, Self> {
        let receiver = self
            .finished
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match receiver.try_recv() {
            Ok(finished) => {
                if let Some(handle) = self.handle.take() {
                    let _ = handle.join();
                }
                Ok(finished)
            }
            Err(TryRecvError::Empty) => Err(self),
            Err(TryRecvError::Disconnected) => {
                panic!("layout worker thread exited without producing a step")
            }
        }
    }
}

/// Run one already-snapshotted step on a new thread. Dropping the returned
/// flight detaches the thread; its engine and snapshot are released when it
/// finishes.
pub(crate) fn spawn(flight: LayoutFlight) -> ThreadFlight {
    let (sender, finished) = sync_channel(1);
    let handle = run(move || {
        let _ = sender.send(flight.run());
    })
    .expect("spawn the layout worker thread");
    ThreadFlight {
        finished: Mutex::new(finished),
        handle: Some(handle),
    }
}

fn run(body: impl FnOnce() + Send + 'static) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("gpui-graph-layout".to_owned())
        .spawn(body)
}
