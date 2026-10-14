//! How many downloads may transfer at once: `DownloadConcurrency` and `TransferLimiter`.

use std::collections::VecDeque;
use std::sync::Arc;

use octo_core::settings::SettingsStore;
use parking_lot::Mutex;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// How many downloads may transfer at once, and the gate that holds them to it.
///
/// Every download lands in a folder of its own (the YouTube staging folder, a Lidarr job
/// folder), so two transfers in flight can never claim each other's file and the setting is
/// safe to follow as it is. Placing files into the library stays one at a time either way.
pub struct DownloadConcurrency {
    state: Arc<ConcurrencyState>,
    transfers: TransferLimiter,
}

struct ConcurrencyState {
    /// The download settings, read at every use.
    settings: Arc<SettingsStore>,
}

impl ConcurrencyState {
    fn current(&self) -> i32 {
        self.settings.current().downloads.effective_parallel_downloads()
    }
}

impl DownloadConcurrency {
    pub fn new(settings: Arc<SettingsStore>) -> Self {
        let state = Arc::new(ConcurrencyState { settings });
        let width = state.clone();
        DownloadConcurrency {
            transfers: TransferLimiter::new(move || width.current()),
            state,
        }
    }

    /// Taken around each transfer, so album walks and hearts outside the queue count too.
    pub fn transfers(&self) -> &TransferLimiter {
        &self.transfers
    }

    pub fn current(&self) -> i32 {
        self.state.current()
    }

    /// Why Current is what it is, in words for the dashboard.
    pub fn why(&self) -> String {
        match self.current() {
            1 => "One at a time, as set.".to_string(),
            n => format!("Up to {n} at once."),
        }
    }
}

/// An async gate whose width is read on every entry and exit, so a live setting change applies
/// to the next transfer and a narrower width drains instead of cutting anything off.
#[derive(Clone)]
pub struct TransferLimiter {
    inner: Arc<LimiterInner>,
}

struct LimiterInner {
    width: Box<dyn Fn() -> i32 + Send + Sync>,
    state: Mutex<LimiterState>,
}

#[derive(Default)]
struct LimiterState {
    in_use: i32,
    waiting: VecDeque<(u64, oneshot::Sender<()>)>,
    next_ticket: u64,
}

/// The transfer was given up while waiting for a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the wait for a transfer slot was cancelled")]
pub struct Cancelled;

impl TransferLimiter {
    pub fn new(width: impl Fn() -> i32 + Send + Sync + 'static) -> Self {
        TransferLimiter {
            inner: Arc::new(LimiterInner {
                width: Box::new(width),
                state: Mutex::new(LimiterState::default()),
            }),
        }
    }

    pub fn in_use(&self) -> i32 {
        self.inner.state.lock().in_use
    }

    /// Waits for a slot. Dropping the returned slot leaves; canceling `token` (or dropping
    /// this future) while it waits gives the place up. A slot handed over in the same instant
    /// as the cancellation is still returned.
    pub async fn enter(&self, token: &CancellationToken) -> Result<TransferSlot, Cancelled> {
        let (ticket, rx) = {
            let mut state = self.inner.state.lock();
            if state.in_use < (self.inner.width)().max(1) {
                state.in_use += 1;
                return Ok(TransferSlot::new(self.inner.clone()));
            }
            let (tx, rx) = oneshot::channel();
            let ticket = state.next_ticket;
            state.next_ticket += 1;
            state.waiting.push_back((ticket, tx));
            (ticket, rx)
        };
        let mut waiter = Waiter {
            inner: self.inner.clone(),
            ticket,
            rx: Some(rx),
        };
        let handed = {
            let rx = waiter.rx.as_mut().expect("set above");
            tokio::select! {
                biased;
                handed = rx => Some(handed),
                _ = token.cancelled() => None,
            }
        };
        match handed {
            Some(handed) => {
                waiter.rx = None;
                // A leaving holder handed its slot over, so in_use already counts this one.
                // The sender is only dropped unsent by this waiter's own give_up.
                handed.expect("a queued sender is only dropped by its own waiter");
                Ok(TransferSlot::new(self.inner.clone()))
            }
            None if waiter.give_up() => Ok(TransferSlot::new(self.inner.clone())),
            None => Err(Cancelled),
        }
    }

    fn leave(inner: &LimiterInner) {
        let mut state = inner.state.lock();
        // Hand the slot straight on while the width allows; the leaver is still counted here.
        while state.in_use <= (inner.width)().max(1) {
            let Some((_, next)) = state.waiting.pop_front() else {
                break;
            };
            if next.send(()).is_ok() {
                return;
            }
        }
        state.in_use -= 1;
    }
}

/// A place in the queue, given up when dropped before a slot arrived.
struct Waiter {
    inner: Arc<LimiterInner>,
    ticket: u64,
    rx: Option<oneshot::Receiver<()>>,
}

impl Waiter {
    /// Leaves the queue. True when a slot had already been handed over, so the caller owns it.
    fn give_up(&mut self) -> bool {
        let Some(mut rx) = self.rx.take() else {
            return false;
        };
        let mut state = self.inner.state.lock();
        if let Some(pos) = state.waiting.iter().position(|(t, _)| *t == self.ticket) {
            state.waiting.remove(pos);
            return false;
        }
        drop(state);
        // Not queued any more: a leaver has popped it, and sent unless the send failed.
        rx.try_recv().is_ok()
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        // A future dropped while waiting must not keep a slot that was handed to it.
        if self.give_up() {
            TransferLimiter::leave(&self.inner);
        }
    }
}

/// One transfer's slot; dropping it leaves the gate.
pub struct TransferSlot {
    inner: Option<Arc<LimiterInner>>,
}

impl TransferSlot {
    fn new(inner: Arc<LimiterInner>) -> Self {
        TransferSlot { inner: Some(inner) }
    }
}

impl Drop for TransferSlot {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            TransferLimiter::leave(&inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octo_core::settings::{AppSettings, DownloadSettings};
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::time::Duration;

    fn concurrency(width: i32) -> DownloadConcurrency {
        DownloadConcurrency::new(Arc::new(SettingsStore::for_tests(AppSettings {
            downloads: DownloadSettings {
                parallel_downloads: width,
                ..Default::default()
            },
            ..Default::default()
        })))
    }

    async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), f)
            .await
            .expect("within 5 s")
    }

    // ---- the transfer gate ----------------------------------------------------------------

    #[tokio::test]
    async fn the_gate_lets_in_its_width_and_the_next_waits() {
        let limiter = TransferLimiter::new(|| 2);
        let none = CancellationToken::new();
        let a = limiter.enter(&none).await.expect("a slot");
        let b = limiter.enter(&none).await.expect("a slot");
        let l = limiter.clone();
        let t = none.clone();
        let c = tokio::spawn(async move { l.enter(&t).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!c.is_finished());
        drop(a);
        drop(within(c).await.expect("joins").expect("a slot"));
        drop(b);
        assert_eq!(limiter.in_use(), 0);
    }

    #[tokio::test]
    async fn widening_lets_a_waiter_in_on_the_next_exit_narrowing_drains() {
        let width = Arc::new(AtomicI32::new(1));
        let w = width.clone();
        let limiter = TransferLimiter::new(move || w.load(Ordering::SeqCst));
        let none = CancellationToken::new();
        let a = limiter.enter(&none).await.expect("a slot");
        let (l, t) = (limiter.clone(), none.clone());
        let b = tokio::spawn(async move { l.enter(&t).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        width.store(3, Ordering::SeqCst);
        let c = within(limiter.enter(&none)).await.expect("room now");
        assert!(
            !b.is_finished(),
            "queued before the widening; let in by the next exit"
        );
        drop(a);
        let b_slot = within(b).await.expect("joins").expect("a slot");
        assert_eq!(limiter.in_use(), 2);

        width.store(1, Ordering::SeqCst);
        let (l, t) = (limiter.clone(), none.clone());
        let d = tokio::spawn(async move { l.enter(&t).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(c);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!d.is_finished(), "still 1 in use, which is the width");
        drop(b_slot);
        drop(within(d).await.expect("joins").expect("a slot"));
        assert_eq!(limiter.in_use(), 0);
    }

    #[tokio::test]
    async fn a_canceled_waiter_never_takes_a_slot() {
        let limiter = TransferLimiter::new(|| 1);
        let none = CancellationToken::new();
        let held = limiter.enter(&none).await.expect("a slot");
        let cts = CancellationToken::new();
        let (l, t) = (limiter.clone(), cts.clone());
        let cancelled = tokio::spawn(async move { l.enter(&t).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        let (l, t) = (limiter.clone(), none.clone());
        let next = tokio::spawn(async move { l.enter(&t).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        cts.cancel();
        assert_eq!(within(cancelled).await.expect("joins").err(), Some(Cancelled));
        drop(held);
        drop(within(next).await.expect("joins").expect("a slot"));
        assert_eq!(limiter.in_use(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn under_load_never_more_than_the_width() {
        for run in 0..50 {
            let limiter = TransferLimiter::new(|| 3);
            let inside = Arc::new(AtomicI32::new(0));
            let most = Arc::new(AtomicI32::new(0));
            let tasks: Vec<_> = (0..20)
                .map(|i| {
                    let (limiter, inside, most) = (limiter.clone(), inside.clone(), most.clone());
                    tokio::spawn(async move {
                        let _slot = limiter.enter(&CancellationToken::new()).await.expect("a slot");
                        let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis((i % 3) as u64)).await;
                        inside.fetch_sub(1, Ordering::SeqCst);
                    })
                })
                .collect();
            for t in tasks {
                t.await.expect("joins");
            }
            let most = most.load(Ordering::SeqCst);
            assert!(most <= 3, "run {run}: {most} at once");
            assert_eq!(limiter.in_use(), 0);
        }
    }

    #[test]
    fn concurrency_is_the_setting_and_says_so() {
        let concurrency = concurrency(4);
        assert_eq!(concurrency.current(), 4);
        assert_eq!(concurrency.why(), "Up to 4 at once.");
    }

    // ---- dropped waiters and the live setting ----------------------------------------------

    #[tokio::test]
    async fn a_dropped_waiter_gives_its_place_to_the_next() {
        let limiter = TransferLimiter::new(|| 1);
        let none = CancellationToken::new();
        let held = limiter.enter(&none).await.expect("a slot");
        let (l, t) = (limiter.clone(), none.clone());
        let abandoned = tokio::spawn(async move { l.enter(&t).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        abandoned.abort();
        let _ = abandoned.await;
        let (l, t) = (limiter.clone(), none.clone());
        let next = tokio::spawn(async move { l.enter(&t).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(held);
        drop(within(next).await.expect("joins").expect("a slot"));
        assert_eq!(limiter.in_use(), 0);
    }

    #[tokio::test]
    async fn the_gate_follows_the_live_setting() {
        let concurrency = concurrency(1);
        assert_eq!(concurrency.why(), "One at a time, as set.");
        concurrency.state.settings.set(AppSettings {
            downloads: DownloadSettings {
                parallel_downloads: 2,
                ..Default::default()
            },
            ..Default::default()
        });
        let none = CancellationToken::new();
        let a = concurrency.transfers().enter(&none).await.expect("a slot");
        let b = within(concurrency.transfers().enter(&none))
            .await
            .expect("a second slot");
        assert_eq!(concurrency.transfers().in_use(), 2);
        drop((a, b));
    }
}
