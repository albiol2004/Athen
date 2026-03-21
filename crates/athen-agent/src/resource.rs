//! Resource monitor (memory, CPU) for agent processes.

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;

use athen_core::error::Result;
use athen_core::traits::agent::{ResourceMonitor, ResourceUsage};

/// Default resource monitor that checks memory and CPU usage.
///
/// On Linux, reads `/proc/self/statm` for memory usage.
/// On other platforms, returns dummy values.
pub struct DefaultResourceMonitor {
    memory_limit_bytes: u64,
    cpu_limit_percent: f32,
    /// Cached flag for whether we are within limits. Updated on each
    /// call to `current_usage()`.
    within_limits: AtomicBool,
}

impl DefaultResourceMonitor {
    /// Create a new resource monitor with the given limits.
    pub fn new(memory_limit_bytes: u64, cpu_limit_percent: f32) -> Self {
        Self {
            memory_limit_bytes,
            cpu_limit_percent,
            within_limits: AtomicBool::new(true),
        }
    }

    /// Returns the configured memory limit in bytes.
    pub fn memory_limit_bytes(&self) -> u64 {
        self.memory_limit_bytes
    }

    /// Returns the configured CPU limit as a percentage.
    pub fn cpu_limit_percent(&self) -> f32 {
        self.cpu_limit_percent
    }

    /// Read memory usage from /proc/self/statm on Linux.
    #[cfg(target_os = "linux")]
    fn read_memory_bytes() -> u64 {
        // /proc/self/statm fields: size resident shared text lib data dt
        // All values are in pages. We want "resident" (field index 1).
        let page_size = 4096u64; // typical page size
        std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|content| {
                content
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(|pages| pages * page_size)
            })
            .unwrap_or(0)
    }

    #[cfg(not(target_os = "linux"))]
    fn read_memory_bytes() -> u64 {
        0
    }
}

#[async_trait]
impl ResourceMonitor for DefaultResourceMonitor {
    async fn current_usage(&self) -> Result<ResourceUsage> {
        let memory_bytes = Self::read_memory_bytes();
        // CPU percent measurement requires sampling over time;
        // return 0.0 as a placeholder. A production implementation
        // would use a background sampler or /proc/self/stat deltas.
        let cpu_percent = 0.0;

        let usage = ResourceUsage {
            memory_bytes,
            cpu_percent,
        };

        let within = usage.memory_bytes <= self.memory_limit_bytes
            && usage.cpu_percent <= self.cpu_limit_percent;
        self.within_limits.store(within, Ordering::Relaxed);

        Ok(usage)
    }

    fn is_within_limits(&self) -> bool {
        self.within_limits.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_resource_monitor_basic() {
        let monitor = DefaultResourceMonitor::new(1_000_000_000, 90.0);
        let usage = monitor.current_usage().await.unwrap();
        // On any platform, memory should be a non-negative value
        assert!(usage.cpu_percent >= 0.0);
        // After calling current_usage, is_within_limits should reflect the check
        assert!(monitor.is_within_limits());
    }

    #[tokio::test]
    async fn test_resource_monitor_exceeds_memory_limit() {
        // Set a very low memory limit that will be exceeded
        let monitor = DefaultResourceMonitor::new(1, 90.0);
        let _usage = monitor.current_usage().await.unwrap();

        // On Linux, our process definitely uses more than 1 byte of memory.
        // On other platforms, read_memory_bytes returns 0, so 0 <= 1 is within limits.
        #[cfg(target_os = "linux")]
        assert!(!monitor.is_within_limits());

        #[cfg(not(target_os = "linux"))]
        assert!(monitor.is_within_limits());
    }

    #[test]
    fn test_limits_accessors() {
        let monitor = DefaultResourceMonitor::new(512_000_000, 75.0);
        assert_eq!(monitor.memory_limit_bytes(), 512_000_000);
        assert!((monitor.cpu_limit_percent() - 75.0).abs() < f32::EPSILON);
    }
}
