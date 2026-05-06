//! Quota-aware fan-out across multiple search providers.
//!
//! Walks providers in priority order, skipping any that hit a rate-limit or
//! quota error recently. Cooldowns are in-memory: on restart we retry every
//! provider once and rediscover its state from the response. That trades a
//! few wasted calls per restart for not having to track the provider's clock
//! ourselves.
//!
//! DDG (or any provider added without a cooldown) acts as the floor — it
//! never enters cooldown, so the chain always has *something* to fall back to.

use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tracing::{info, warn};

use athen_core::error::{AthenError, Result};

use super::{SearchResult, WebSearchProvider};

/// One slot in the multi-provider chain.
pub struct ProviderSlot {
    inner: std::sync::Arc<dyn WebSearchProvider>,
    /// `None` → never enters cooldown (use for DDG / always-available floors).
    /// `Some` → cooldown windows are honored; quota errors back off until next
    /// month boundary, rate limits back off for a shorter window.
    cooldownable: bool,
    /// Set by [`MultiSearchProvider::search`] when this provider returns a
    /// rate-limit / quota error. Reads are best-effort; we hold the lock only
    /// for a `clone` of the timestamp.
    cooldown_until: Mutex<Option<Instant>>,
}

impl ProviderSlot {
    pub fn keyed(inner: std::sync::Arc<dyn WebSearchProvider>) -> Self {
        Self {
            inner,
            cooldownable: true,
            cooldown_until: Mutex::new(None),
        }
    }

    pub fn floor(inner: std::sync::Arc<dyn WebSearchProvider>) -> Self {
        Self {
            inner,
            cooldownable: false,
            cooldown_until: Mutex::new(None),
        }
    }

    fn in_cooldown(&self) -> bool {
        if !self.cooldownable {
            return false;
        }
        let guard = self.cooldown_until.lock().expect("cooldown lock poisoned");
        match *guard {
            Some(until) => Instant::now() < until,
            None => false,
        }
    }

    fn set_cooldown(&self, dur: Duration) {
        if !self.cooldownable {
            return;
        }
        let until = Instant::now() + dur;
        *self.cooldown_until.lock().expect("cooldown lock poisoned") = Some(until);
    }
}

pub struct MultiSearchProvider {
    slots: Vec<ProviderSlot>,
    /// 1-based index (into `slots`) of the provider that handled the
    /// most recent call. `0` means "no call yet, fall back to the
    /// wrapper name". Stored atomically so [`WebSearchProvider::last_used`]
    /// stays cheap and lock-free for read-heavy agent paths.
    last_used_idx: std::sync::atomic::AtomicUsize,
}

impl MultiSearchProvider {
    pub fn new(slots: Vec<ProviderSlot>) -> Self {
        Self {
            slots,
            last_used_idx: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn record_used_index(&self, idx: usize) {
        self.last_used_idx
            .store(idx.saturating_add(1), Ordering::Relaxed);
    }
}

#[async_trait]
impl WebSearchProvider for MultiSearchProvider {
    fn name(&self) -> &'static str {
        "multi"
    }

    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>> {
        let mut last_err: Option<AthenError> = None;
        let mut last_idx: Option<usize> = None;
        for (idx, slot) in self.slots.iter().enumerate() {
            if slot.in_cooldown() {
                info!(provider = slot.inner.name(), "skipping (in cooldown)");
                continue;
            }
            info!(provider = slot.inner.name(), "trying provider");
            match slot.inner.search(query, max_results).await {
                Ok(results) => {
                    info!(
                        provider = slot.inner.name(),
                        hits = results.len(),
                        "provider answered"
                    );
                    self.record_used_index(idx);
                    return Ok(results);
                }
                Err(e) => {
                    let msg = e.to_string();
                    let cooldown = classify_error(&msg);
                    if let Some(dur) = cooldown {
                        warn!(
                            provider = slot.inner.name(),
                            cooldown_secs = dur.as_secs(),
                            error = %msg,
                            "provider exhausted; cooling down"
                        );
                        slot.set_cooldown(dur);
                    } else {
                        // Non-cooldown error — provider stays armed so the
                        // next call retries it. Bumped to info so users can
                        // see auth / region / format errors without
                        // RUST_LOG=debug.
                        info!(
                            provider = slot.inner.name(),
                            error = %msg,
                            "provider failed (no cooldown), trying next"
                        );
                    }
                    last_err = Some(e);
                    last_idx = Some(idx);
                }
            }
        }
        // No success — record whichever provider produced the final
        // error so observers can still see who tried last.
        if let Some(idx) = last_idx {
            self.record_used_index(idx);
        }
        Err(last_err.unwrap_or_else(|| AthenError::Other("no search providers configured".into())))
    }

    fn last_used(&self) -> &'static str {
        let raw = self.last_used_idx.load(Ordering::Relaxed);
        if raw == 0 {
            return self.name();
        }
        let idx = raw - 1;
        match self.slots.get(idx) {
            Some(slot) => slot.inner.name(),
            None => self.name(),
        }
    }
}

/// Inspect a provider error and decide how long to cool that provider down
/// for. Heuristic but robust — we look at HTTP status codes and common
/// quota-language so providers don't all need to return distinguished errors.
fn classify_error(msg: &str) -> Option<Duration> {
    let lower = msg.to_ascii_lowercase();

    // Hard quota — usually month-bounded. Back off until ~next day; user can
    // restart Athen to retry sooner if a billing event resets faster.
    if lower.contains("http 402")
        || lower.contains("http 403")
        || lower.contains("quota")
        || lower.contains("exceeded")
        || lower.contains("subscription")
    {
        return Some(Duration::from_secs(24 * 60 * 60));
    }

    // Rate limit — short backoff, the next retry might already succeed.
    if lower.contains("http 429")
        || lower.contains("rate limit")
        || lower.contains("rate-limit")
        || lower.contains("rate-limited")
        || lower.contains("too many requests")
    {
        return Some(Duration::from_secs(15 * 60));
    }

    // Other failures (network, JSON decode, server 5xx) — don't penalize the
    // provider; the next call probably works.
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Stub {
        name: &'static str,
        calls: AtomicUsize,
        result: std::sync::Mutex<std::result::Result<Vec<SearchResult>, String>>,
    }

    impl Stub {
        fn ok(name: &'static str, hits: Vec<SearchResult>) -> Arc<Self> {
            Arc::new(Self {
                name,
                calls: AtomicUsize::new(0),
                result: std::sync::Mutex::new(Ok(hits)),
            })
        }
        fn err(name: &'static str, err: &str) -> Arc<Self> {
            Arc::new(Self {
                name,
                calls: AtomicUsize::new(0),
                result: std::sync::Mutex::new(Err(err.to_string())),
            })
        }
    }

    #[async_trait]
    impl WebSearchProvider for Stub {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn search(&self, _q: &str, _n: usize) -> Result<Vec<SearchResult>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            match &*self.result.lock().unwrap() {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(AthenError::Other(e.clone())),
            }
        }
    }

    fn hit(title: &str) -> SearchResult {
        SearchResult {
            title: title.into(),
            url: "https://example.com".into(),
            snippet: "".into(),
        }
    }

    #[tokio::test]
    async fn first_provider_wins_when_healthy() {
        let a = Stub::ok("a", vec![hit("from-a")]);
        let b = Stub::ok("b", vec![hit("from-b")]);
        let multi = MultiSearchProvider::new(vec![
            ProviderSlot::keyed(a.clone()),
            ProviderSlot::keyed(b.clone()),
        ]);
        let res = multi.search("q", 5).await.unwrap();
        assert_eq!(res[0].title, "from-a");
        assert_eq!(a.calls.load(Ordering::Relaxed), 1);
        assert_eq!(b.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn rate_limit_falls_through_and_cools_down() {
        let a = Stub::err("a", "tavily HTTP 429: too many requests");
        let b = Stub::ok("b", vec![hit("from-b")]);
        let multi = MultiSearchProvider::new(vec![
            ProviderSlot::keyed(a.clone()),
            ProviderSlot::keyed(b.clone()),
        ]);
        let res = multi.search("q", 5).await.unwrap();
        assert_eq!(res[0].title, "from-b");
        // Second call must skip a entirely (it's in cooldown).
        let _ = multi.search("q", 5).await.unwrap();
        assert_eq!(
            a.calls.load(Ordering::Relaxed),
            1,
            "a should be skipped on retry"
        );
        assert_eq!(b.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn quota_error_marks_provider_exhausted() {
        let a = Stub::err("a", "brave HTTP 402: subscription exceeded");
        let b = Stub::ok("b", vec![hit("from-b")]);
        let multi = MultiSearchProvider::new(vec![
            ProviderSlot::keyed(a.clone()),
            ProviderSlot::keyed(b.clone()),
        ]);
        let _ = multi.search("q", 5).await.unwrap();
        let _ = multi.search("q", 5).await.unwrap();
        assert_eq!(a.calls.load(Ordering::Relaxed), 1);
        assert_eq!(b.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn floor_provider_never_cools_down() {
        let ddg = Stub::err("ddg", "duckduckgo rate-limited (HTTP 202)");
        let multi = MultiSearchProvider::new(vec![ProviderSlot::floor(ddg.clone())]);
        // Two calls in a row even after a rate-limit error — floor never cools.
        let _ = multi.search("q", 5).await;
        let _ = multi.search("q", 5).await;
        assert_eq!(ddg.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn last_used_reports_answering_provider() {
        let a = Stub::err("a", "tavily HTTP 429: too many requests");
        let b = Stub::ok("b", vec![hit("from-b")]);
        let multi = MultiSearchProvider::new(vec![
            ProviderSlot::keyed(a.clone()),
            ProviderSlot::keyed(b.clone()),
        ]);
        // Before any call: falls back to the wrapper's own name.
        assert_eq!(multi.last_used(), "multi");
        let _ = multi.search("q", 5).await.unwrap();
        // After: reports the provider that actually answered, not "multi".
        assert_eq!(multi.last_used(), "b");
    }

    #[tokio::test]
    async fn returns_last_error_when_all_fail() {
        let a = Stub::err("a", "boom-a");
        let b = Stub::err("b", "boom-b");
        let multi = MultiSearchProvider::new(vec![ProviderSlot::keyed(a), ProviderSlot::keyed(b)]);
        let err = multi.search("q", 5).await.unwrap_err();
        assert!(err.to_string().contains("boom-b"));
    }

    #[test]
    fn classify_recognizes_rate_limit_phrasings() {
        assert!(classify_error("HTTP 429").is_some());
        assert!(classify_error("rate-limited").is_some());
        assert!(classify_error("Too Many Requests").is_some());
        assert!(classify_error("network unreachable").is_none());
    }

    #[test]
    fn classify_recognizes_quota_phrasings() {
        assert!(classify_error("HTTP 402: payment required").is_some());
        assert!(classify_error("monthly quota exceeded").is_some());
        assert!(classify_error("subscription invalid").is_some());
    }
}
