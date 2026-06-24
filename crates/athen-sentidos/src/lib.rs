//! Sense monitors (Sentidos) for Athen.
//!
//! Each monitor polls an external source and produces normalized SenseEvents.
//! The [`SenseRunner`] provides shared polling infrastructure that can drive
//! any [`SenseMonitor`] implementation.

pub mod calendar;
pub mod email;
pub mod email_send;
pub mod messaging;
pub mod pdf_extract;
pub mod telegram;
pub mod telegram_send;
pub mod user_input;

use athen_core::error::{AthenError, Result};
use athen_core::event::SenseEvent;
use athen_core::traits::sense::SenseMonitor;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

/// First respawn delay used by [`SenseRunner::run_supervised`].
const SUPERVISION_INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Ceiling for [`SenseRunner::run_supervised`]'s exponential backoff. A
/// permanently-failing monitor (e.g. its event channel is gone for good)
/// settles here instead of retrying in a tight loop.
const SUPERVISION_BACKOFF_CAP: Duration = Duration::from_secs(60);

/// Runs a [`SenseMonitor`] in a polling loop, forwarding events through a channel.
///
/// The runner calls `monitor.poll()` on each tick, sends resulting events
/// through `event_sender`, then sleeps for `monitor.poll_interval()`.
/// It exits cleanly when the `shutdown` signal fires.
pub struct SenseRunner<M: SenseMonitor> {
    monitor: M,
    event_sender: mpsc::Sender<SenseEvent>,
}

impl<M: SenseMonitor> SenseRunner<M> {
    /// Create a new runner for the given monitor and event channel.
    pub fn new(monitor: M, event_sender: mpsc::Sender<SenseEvent>) -> Self {
        Self {
            monitor,
            event_sender,
        }
    }

    /// Run the polling loop until a shutdown signal is received.
    ///
    /// On each iteration the runner:
    /// 1. Calls `monitor.poll()` to collect events.
    /// 2. Sends each event through `event_sender`.
    /// 3. Sleeps for `monitor.poll_interval()`.
    /// 4. Checks the shutdown signal.
    ///
    /// After shutdown, `monitor.shutdown()` is called for cleanup.
    pub async fn run(&self, mut shutdown: broadcast::Receiver<()>) -> Result<()> {
        let interval = self.monitor.poll_interval();
        let sense_id = self.monitor.sense_id().to_string();

        tracing::info!(sense = %sense_id, "SenseRunner starting");

        loop {
            // Poll for events.
            match self.monitor.poll().await {
                Ok(events) => {
                    for event in events {
                        if let Err(e) = self.event_sender.send(event).await {
                            tracing::error!(
                                sense = %sense_id,
                                "Failed to send event: {e}"
                            );
                            // Receiver dropped — no point continuing.
                            self.monitor.shutdown().await?;
                            return Err(AthenError::Other(format!(
                                "Event channel closed for sense '{sense_id}'"
                            )));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        sense = %sense_id,
                        "Poll error: {e}"
                    );
                    // Continue running; transient errors are expected.
                }
            }

            // Sleep, but break early on shutdown.
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = shutdown.recv() => {
                    tracing::info!(sense = %sense_id, "Shutdown signal received");
                    break;
                }
            }
        }

        self.monitor.shutdown().await?;
        tracing::info!(sense = %sense_id, "SenseRunner stopped");
        Ok(())
    }

    /// Get a reference to the underlying monitor.
    pub fn monitor(&self) -> &M {
        &self.monitor
    }

    /// Supervise [`Self::run`] so the monitor never silently goes deaf.
    ///
    /// ## Supervision contract
    ///
    /// `run` exits cleanly (`Ok`) only on a shutdown signal; any `Err`
    /// (today: the event channel was dropped) is an *unexpected* exit. On an
    /// unexpected exit this method logs the cause loudly and re-runs after a
    /// bounded exponential backoff (`1s → 2s → … → 60s` cap) so a permanently
    /// broken monitor settles at the ceiling instead of hot-looping. A clean
    /// shutdown stops without respawning, so supervision never fights a
    /// deliberate teardown.
    ///
    /// Each attempt subscribes a **fresh** receiver from `shutdown_tx`, so a
    /// shutdown fired between attempts is observed on the next subscribe and
    /// ends supervision immediately. Pass the *sender* (not a receiver) so the
    /// supervisor can keep re-subscribing across restarts.
    pub async fn run_supervised(&self, shutdown_tx: broadcast::Sender<()>) {
        let sense_id = self.monitor.sense_id().to_string();
        let mut backoff = SUPERVISION_INITIAL_BACKOFF;
        // A dedicated long-lived receiver used only to race the backoff sleep,
        // so a shutdown fired *while we're backing off* is observed instead of
        // missed (a fresh broadcast subscribe won't receive an already-sent
        // pulse). Kept across iterations.
        let mut backoff_shutdown = shutdown_tx.subscribe();

        loop {
            // Each attempt subscribes a fresh receiver for the inner `run`.
            // If the host drops the sender entirely (app teardown), the next
            // `recv()` inside `run` returns `Closed`, which `run` also treats
            // as a clean stop — so we never respawn into a torn-down host.
            let shutdown_rx = shutdown_tx.subscribe();

            match self.run(shutdown_rx).await {
                Ok(()) => {
                    // Clean stop — `run` returns Ok only on a shutdown signal
                    // (or a closed shutdown channel). Never respawn.
                    tracing::info!(
                        sense = %sense_id,
                        "Supervised sense exited cleanly; not respawning"
                    );
                    return;
                }
                Err(e) => {
                    tracing::error!(
                        sense = %sense_id,
                        "Supervised sense exited unexpectedly: {e}; respawning after {:?}",
                        backoff
                    );
                }
            }

            // Back off before respawning, but bail immediately if a shutdown
            // arrives (or the channel closes) during the wait — otherwise a
            // deliberate teardown would be delayed by up to the full backoff,
            // and a shutdown fired now would be missed by the next subscribe.
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = backoff_shutdown.recv() => {
                    tracing::info!(
                        sense = %sense_id,
                        "Supervised sense shutdown during backoff; not respawning"
                    );
                    return;
                }
            }
            backoff = (backoff * 2).min(SUPERVISION_BACKOFF_CAP);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_input::UserInputMonitor;
    use std::time::Duration;

    #[tokio::test]
    async fn runner_polls_and_sends_events() {
        let monitor = UserInputMonitor::new(16);
        let tx = monitor.sender();

        let (event_tx, mut event_rx) = mpsc::channel(16);
        let runner = SenseRunner::new(monitor, event_tx);

        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);

        // Send a message before starting the runner.
        tx.send("test command".to_string()).await.unwrap();

        // Run the runner in a background task.
        let handle = tokio::spawn(async move { runner.run(shutdown_rx).await });

        // Wait for the event to arrive.
        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timed out waiting for event")
            .expect("channel closed");

        assert_eq!(
            event.content.body,
            serde_json::Value::String("test command".to_string())
        );

        // Shut down.
        let _ = shutdown_tx.send(());
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn runner_stops_on_shutdown_signal() {
        let monitor = UserInputMonitor::new(16);
        let (event_tx, _event_rx) = mpsc::channel(16);
        let runner = SenseRunner::new(monitor, event_tx);

        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);

        let handle = tokio::spawn(async move { runner.run(shutdown_rx).await });

        // Give the runner a moment to start polling.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Signal shutdown.
        let _ = shutdown_tx.send(());

        // The runner should exit within a reasonable time.
        let result = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("runner did not shut down in time");

        result.unwrap().unwrap();
    }

    #[tokio::test]
    async fn runner_forwards_multiple_events() {
        let monitor = UserInputMonitor::new(16);
        let tx = monitor.sender();

        let (event_tx, mut event_rx) = mpsc::channel(16);
        let runner = SenseRunner::new(monitor, event_tx);

        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);

        // Queue multiple messages.
        tx.send("first".to_string()).await.unwrap();
        tx.send("second".to_string()).await.unwrap();

        let handle = tokio::spawn(async move { runner.run(shutdown_rx).await });

        // Collect two events.
        let e1 = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let e2 = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            e1.content.body,
            serde_json::Value::String("first".to_string())
        );
        assert_eq!(
            e2.content.body,
            serde_json::Value::String("second".to_string())
        );

        let _ = shutdown_tx.send(());
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn run_supervised_stops_on_shutdown_signal() {
        // A clean shutdown must end supervision (no respawn loop).
        let monitor = UserInputMonitor::new(16);
        let (event_tx, _event_rx) = mpsc::channel(16);
        let runner = SenseRunner::new(monitor, event_tx);

        let (shutdown_tx, _) = broadcast::channel(1);
        let shutdown_tx2 = shutdown_tx.clone();

        let handle = tokio::spawn(async move { runner.run_supervised(shutdown_tx2).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = shutdown_tx.send(());

        // Supervisor returns (does not respawn) within a reasonable time.
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("supervisor did not stop on shutdown")
            .unwrap();
    }

    #[tokio::test]
    async fn run_supervised_respawns_after_channel_drop_then_stops() {
        // Drop the event receiver so `run` fails with a closed-channel error
        // (unexpected exit). The supervisor must log + back off + respawn,
        // and then stop cleanly once shutdown fires.
        let monitor = UserInputMonitor::new(16);
        let tx = monitor.sender();
        let (event_tx, event_rx) = mpsc::channel(1);
        let runner = SenseRunner::new(monitor, event_tx);

        let (shutdown_tx, _) = broadcast::channel(1);
        let shutdown_tx2 = shutdown_tx.clone();

        // Queue a message and drop the receiver so the first send fails.
        tx.send("boom".to_string()).await.unwrap();
        drop(event_rx);

        let handle = tokio::spawn(async move { runner.run_supervised(shutdown_tx2).await });

        // Let it fail at least once and enter backoff (initial backoff is 1s).
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Fire shutdown; the supervisor observes it on the next attempt and
        // stops without looping forever.
        let _ = shutdown_tx.send(());

        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("supervisor did not stop after channel drop + shutdown")
            .unwrap();
    }
}
