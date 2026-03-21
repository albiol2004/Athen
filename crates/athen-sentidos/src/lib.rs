//! Sense monitors (Sentidos) for Athen.
//!
//! Each monitor polls an external source and produces normalized SenseEvents.
//! The [`SenseRunner`] provides shared polling infrastructure that can drive
//! any [`SenseMonitor`] implementation.

pub mod calendar;
pub mod email;
pub mod messaging;
pub mod user_input;

use athen_core::error::{AthenError, Result};
use athen_core::event::SenseEvent;
use athen_core::traits::sense::SenseMonitor;
use tokio::sync::{broadcast, mpsc};

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
}
