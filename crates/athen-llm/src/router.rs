//! Profile-based routing with failover and budget enforcement.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tracing::{debug, warn};

use athen_core::config::ProfileConfig;
use athen_core::error::{AthenError, Result};
use athen_core::llm::{BudgetStatus, LlmRequest, LlmResponse, LlmStream, ModelProfile};
use athen_core::traits::llm::{LlmProvider, LlmRouter};

use crate::budget::BudgetTracker;

// ---------------------------------------------------------------------------
// Retry / backoff policy
// ---------------------------------------------------------------------------

/// Maximum number of attempts against a *single* provider before failover
/// advances to the next provider/tier. `1` would disable retries; `3` means
/// the original try plus up to two retries on a transient error.
const MAX_ATTEMPTS_PER_PROVIDER: u32 = 3;

/// Base delay for the first backoff (attempt 1 → ~`BASE`, attempt 2 → ~`2*BASE`).
const BACKOFF_BASE: Duration = Duration::from_millis(500);

/// Hard cap on a single computed backoff delay, before jitter. Keeps the
/// exponential growth bounded.
const BACKOFF_CAP: Duration = Duration::from_secs(8);

/// Hard cap on a provider-supplied `Retry-After` hint. A hostile or buggy
/// header (e.g. `Retry-After: 86400`) must never hang the agent — we honor
/// the hint only up to this ceiling.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(30);

/// Cheap jitter source in `[0.0, 1.0)` without pulling in the `rand` crate.
/// Seeds an xorshift from the current nanosecond clock — good enough to
/// decorrelate retry timing across callers (full-jitter's only goal); this is
/// not used for anything security-sensitive.
fn jitter01() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut x = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15)
        | 1;
    // xorshift64
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    // Map to [0, 1) using the high 53 bits.
    ((x >> 11) as f64) / ((1u64 << 53) as f64)
}

/// Compute the backoff delay before the *next* attempt.
///
/// * `attempt` is the number of the attempt that just failed (1-based): the
///   first failure schedules the delay before attempt 2.
/// * `retry_after` is a provider-supplied `Retry-After` hint (already in
///   seconds); when present it takes precedence (capped at [`RETRY_AFTER_CAP`]),
///   because the provider knows better than our exponential schedule.
/// * Otherwise we use full-jitter exponential backoff: the deterministic
///   ceiling is `min(BASE * 2^(attempt-1), CAP)`, and the actual delay is a
///   uniform random value in `[0, ceiling]`. Full jitter avoids the
///   thundering-herd retry-synchronization problem.
///
/// `rand01` is the jitter source in `[0.0, 1.0)` — injected so the schedule is
/// deterministically testable; production passes [`jitter01`].
fn backoff_delay(attempt: u32, retry_after: Option<Duration>, rand01: f64) -> Duration {
    if let Some(ra) = retry_after {
        return ra.min(RETRY_AFTER_CAP);
    }
    let exp = attempt.saturating_sub(1).min(16);
    let ceiling_ms = (BACKOFF_BASE.as_millis() as u64)
        .saturating_mul(1u64 << exp)
        .min(BACKOFF_CAP.as_millis() as u64);
    let jittered = (ceiling_ms as f64) * rand01.clamp(0.0, 1.0);
    Duration::from_millis(jittered as u64)
}

// ---------------------------------------------------------------------------
// Circuit breaker
// ---------------------------------------------------------------------------

/// Tracks the health state of a single provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation — requests flow through.
    Closed,
    /// Too many failures — requests are rejected immediately.
    Open,
    /// Trial period — allow one request to see if the provider recovered.
    HalfOpen,
}

/// Per-provider circuit breaker that prevents hammering a failing service.
pub struct CircuitBreaker {
    state: CircuitState,
    failure_count: u32,
    success_count: u32,
    failure_threshold: u32,
    success_threshold: u32,
    timeout: Duration,
    last_failure: Option<Instant>,
}

impl CircuitBreaker {
    /// Create a circuit breaker with default thresholds.
    pub fn new() -> Self {
        Self {
            state: CircuitState::Closed,
            failure_count: 0,
            success_count: 0,
            failure_threshold: 5,
            success_threshold: 2,
            timeout: Duration::from_secs(60),
            last_failure: None,
        }
    }

    /// Create with custom thresholds.
    pub fn with_thresholds(
        failure_threshold: u32,
        success_threshold: u32,
        timeout: Duration,
    ) -> Self {
        Self {
            state: CircuitState::Closed,
            failure_count: 0,
            success_count: 0,
            failure_threshold,
            success_threshold,
            timeout,
            last_failure: None,
        }
    }

    /// Whether the circuit currently allows a request through.
    pub fn allows_request(&mut self) -> bool {
        match self.state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                // Check if timeout elapsed — transition to half-open.
                if let Some(last) = self.last_failure {
                    if last.elapsed() >= self.timeout {
                        self.state = CircuitState::HalfOpen;
                        self.success_count = 0;
                        true
                    } else {
                        false
                    }
                } else {
                    // Should not happen, but be safe.
                    self.state = CircuitState::Closed;
                    true
                }
            }
            CircuitState::HalfOpen => true,
        }
    }

    /// Record a successful call.
    pub fn record_success(&mut self) {
        match self.state {
            CircuitState::HalfOpen => {
                self.success_count += 1;
                if self.success_count >= self.success_threshold {
                    self.state = CircuitState::Closed;
                    self.failure_count = 0;
                    self.success_count = 0;
                }
            }
            CircuitState::Closed => {
                // Reset consecutive failures on any success.
                self.failure_count = 0;
            }
            CircuitState::Open => {
                // Shouldn't happen — we wouldn't call if open.
            }
        }
    }

    /// Record a failed call.
    pub fn record_failure(&mut self) {
        self.last_failure = Some(Instant::now());
        match self.state {
            CircuitState::Closed => {
                self.failure_count += 1;
                if self.failure_count >= self.failure_threshold {
                    self.state = CircuitState::Open;
                }
            }
            CircuitState::HalfOpen => {
                // Any failure in half-open → back to open.
                self.state = CircuitState::Open;
                self.success_count = 0;
            }
            CircuitState::Open => {
                // Already open, nothing to do.
            }
        }
    }

    /// Current state.
    pub fn state(&self) -> CircuitState {
        self.state
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Stream wrapper that updates the circuit breaker on terminal events
// ---------------------------------------------------------------------------

/// Wraps a provider [`LlmStream`] so the provider's circuit breaker records the
/// real outcome of the byte stream, not just the HTTP-200 connection:
///
/// * the first `Err` chunk records a FAILURE (so a provider that returns 200 and
///   then resets is counted toward opening the breaker), and
/// * a clean end-of-stream (poll returns `None` having seen no error) records a
///   SUCCESS.
///
/// Recording happens at most once per stream (`recorded`), so a consumer that
/// keeps polling past the terminal item does not double-count.
struct BreakerStream {
    inner: LlmStream,
    breakers: Arc<Mutex<HashMap<String, CircuitBreaker>>>,
    provider_id: String,
    saw_error: bool,
    recorded: bool,
    /// Budget tracker, shared with the router. Streamed usage rides on the
    /// terminal usage-bearing chunk (`LlmChunk.usage`), which we stash here
    /// as it flows past and record against the budget on CLEAN completion.
    /// Mirrors how `route_with_failover` calls `record_usage` for the
    /// non-streaming path.
    budget_tracker: Arc<BudgetTracker>,
    /// Last `Some(usage)` observed on the stream. Recorded only on a clean
    /// end-of-stream — a stream that errored returns `Err` and the executor
    /// falls back to a non-streaming `route()` that records its own usage, so
    /// recording here too would double-count.
    pending_usage: Option<athen_core::llm::TokenUsage>,
}

impl BreakerStream {
    fn record_failure(&mut self) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        let mut bs = self.breakers.lock().unwrap();
        if let Some(b) = bs.get_mut(&self.provider_id) {
            b.record_failure();
        }
        warn!(
            provider = %self.provider_id,
            "streaming provider errored mid-stream; recorded breaker failure"
        );
    }

    fn record_success(&mut self) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        {
            let mut bs = self.breakers.lock().unwrap();
            if let Some(b) = bs.get_mut(&self.provider_id) {
                b.record_success();
            }
        }
        // Record streamed token usage against the budget — but only here, on
        // a clean completion. A mid-stream error takes the `record_failure`
        // path and never reaches this, so the executor's non-streaming
        // fallback owns the spend and we don't double-count.
        if let Some(usage) = self.pending_usage.take() {
            self.budget_tracker.record_usage(&usage);
        }
    }
}

impl futures::Stream for BreakerStream {
    type Item = Result<athen_core::llm::LlmChunk>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                // Stash the usage-bearing chunk's usage; recorded on clean end.
                if let Some(usage) = chunk.usage.clone() {
                    this.pending_usage = Some(usage);
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.saw_error = true;
                this.record_failure();
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                // End of stream. Clean end with no error → success.
                if !this.saw_error {
                    this.record_success();
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Default implementation of [`LlmRouter`] with profile-based routing,
/// failover chains, circuit breakers, and budget enforcement.
pub struct DefaultLlmRouter {
    providers: HashMap<String, Box<dyn LlmProvider>>,
    profiles: HashMap<ModelProfile, ProfileConfig>,
    /// `Arc` so the streaming `BreakerStream` wrapper can record streamed
    /// usage against the same budget on clean completion. The public
    /// constructor still takes a `BudgetTracker` by value and wraps it.
    budget_tracker: Arc<BudgetTracker>,
    circuit_breakers: Arc<Mutex<HashMap<String, CircuitBreaker>>>,
}

impl DefaultLlmRouter {
    /// Build a new router.
    ///
    /// * `providers` — keyed by provider id (e.g. `"anthropic"`).
    /// * `profiles` — maps each model profile to its priority list of provider ids.
    /// * `budget_tracker` — shared budget state.
    pub fn new(
        providers: HashMap<String, Box<dyn LlmProvider>>,
        profiles: HashMap<ModelProfile, ProfileConfig>,
        budget_tracker: BudgetTracker,
    ) -> Self {
        let breaker_map: HashMap<String, CircuitBreaker> = providers
            .keys()
            .map(|id| (id.clone(), CircuitBreaker::new()))
            .collect();
        Self {
            providers,
            profiles,
            budget_tracker: Arc::new(budget_tracker),
            circuit_breakers: Arc::new(Mutex::new(breaker_map)),
        }
    }

    /// Build a router with custom circuit breaker settings per provider.
    pub fn with_circuit_breakers(self, breakers: HashMap<String, CircuitBreaker>) -> Self {
        *self.circuit_breakers.lock().unwrap() = breakers;
        self
    }

    /// True if any registered provider claims vision support. Used by the
    /// sense-event executor path to decide whether inlining attachment
    /// images is worth the bytes — text-only providers reject Multimodal
    /// requests, so we drop image inlining and fall back to metadata.
    pub fn any_provider_supports_vision(&self) -> bool {
        self.providers.values().any(|p| p.supports_vision())
    }

    /// Returns the priority list of provider keys for `profile`, or an
    /// empty slice if the profile isn't registered. Used by callers (and
    /// tests) that need to verify which slug-keyed provider instance a
    /// tier maps to — in particular, the arc pinning path needs to
    /// assert that every tier collapses to the pinned slug's single key.
    pub fn profile_provider_keys(&self, profile: ModelProfile) -> &[String] {
        self.profiles
            .get(&profile)
            .map(|c| c.priority.as_slice())
            .unwrap_or(&[])
    }

    /// Current circuit-breaker state for a provider id, if registered. Used by
    /// tests to assert that a mid-stream failure was actually recorded.
    #[cfg(test)]
    pub(crate) fn breaker_state(&self, provider_id: &str) -> Option<CircuitState> {
        self.circuit_breakers
            .lock()
            .unwrap()
            .get(provider_id)
            .map(|b| b.state())
    }

    /// True if any registered provider supports native document/PDF input
    /// (Anthropic document blocks, Gemini `application/pdf` inlineData).
    /// Until `MessageContent` grows a Document variant, this is purely
    /// informational — the executor still falls back to inlining the
    /// extracted text sidecar — but it lets us log a "model could have
    /// rendered the PDF natively" hint, and it's the seam future native
    /// document inlining will key off of.
    pub fn any_provider_supports_documents(&self) -> bool {
        self.providers.values().any(|p| p.supports_documents())
    }

    /// Try each provider in the priority list for the requested profile (streaming).
    async fn route_streaming_with_failover(&self, request: &LlmRequest) -> Result<LlmStream> {
        let profile_config = self.profiles.get(&request.profile).ok_or_else(|| {
            AthenError::Config(format!(
                "no profile configuration for {:?}",
                request.profile
            ))
        })?;

        let priority = &profile_config.priority;
        let mut last_error: Option<AthenError> = None;

        for provider_id in priority {
            // Check circuit breaker
            {
                let mut breakers = self.circuit_breakers.lock().unwrap();
                let breaker = breakers.entry(provider_id.clone()).or_default();
                if !breaker.allows_request() {
                    debug!(
                        provider = %provider_id,
                        "circuit breaker open, skipping provider for streaming"
                    );
                    continue;
                }
            }

            let provider = match self.providers.get(provider_id) {
                Some(p) => p,
                None => {
                    warn!(provider = %provider_id, "provider not registered, skipping");
                    continue;
                }
            };

            // Retry only the CONNECTION attempt on a transient error — you
            // can't retry mid-stream (the BreakerStream wrapper records a
            // mid-stream failure and the executor falls back to non-streaming).
            // This mirrors the non-streaming retry loop.
            let mut attempt: u32 = 0;
            let connect_outcome = loop {
                attempt += 1;
                match provider.complete_streaming(request).await {
                    Ok(stream) => break Ok(stream),
                    Err(e) => {
                        let retryable = e.is_retryable();
                        if retryable && attempt < MAX_ATTEMPTS_PER_PROVIDER {
                            let retry_after = e.retry_after_secs().map(Duration::from_secs);
                            let delay = backoff_delay(attempt, retry_after, jitter01());
                            warn!(
                                provider = %provider_id,
                                attempt,
                                delay_ms = delay.as_millis() as u64,
                                error = %e,
                                "transient streaming-connect error, retrying after backoff"
                            );
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        break Err(e);
                    }
                }
            };

            match connect_outcome {
                Ok(stream) => {
                    // Connecting (HTTP 200) is NOT yet a success: a provider can
                    // return 200 then immediately reset the byte stream. Recording
                    // success here would keep the circuit breaker closed forever
                    // even on a provider that always resets mid-stream, and
                    // failover would never advance. Instead, wrap the stream so the
                    // breaker is updated on TERMINAL events:
                    //   * first `Err` chunk  -> record_failure (counts toward the
                    //     breaker; the executor's stream consumer also bails on it
                    //     so the partial result is never served as final)
                    //   * clean end-of-stream -> record_success
                    // This mirrors how `route_with_failover` records success only
                    // after a complete non-streaming response.
                    let breakers = Arc::clone(&self.circuit_breakers);
                    let provider_id = provider_id.clone();
                    let budget = Arc::clone(&self.budget_tracker);
                    let wrapped =
                        Self::wrap_stream_for_breaker(stream, breakers, provider_id, budget);
                    return Ok(wrapped);
                }
                Err(e) => {
                    warn!(
                        provider = %provider_id,
                        error = %e,
                        "streaming provider call failed, trying next"
                    );
                    {
                        let mut breakers = self.circuit_breakers.lock().unwrap();
                        if let Some(breaker) = breakers.get_mut(provider_id) {
                            breaker.record_failure();
                        }
                    }
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| AthenError::LlmProvider {
            provider: "router".into(),
            message: format!(
                "all providers exhausted for streaming profile {:?}",
                request.profile
            ),
        }))
    }

    /// Wrap a provider stream so the provider's circuit breaker is updated on
    /// terminal events: a chunk-level error records a failure (and the breaker
    /// counts it toward opening the circuit), while a clean end-of-stream records
    /// a success. Success is recorded at most once per stream.
    ///
    /// Note on failover scope: once the byte stream is handed back to the caller
    /// we can no longer transparently switch to the next provider for *this*
    /// stream — the executor's consumer bails on the first chunk error and falls
    /// back to a non-streaming `route()` call, which re-enters the router. Because
    /// the mid-stream error is now recorded as a breaker failure, repeated resets
    /// trip the breaker open and that non-streaming retry (and subsequent
    /// streaming attempts) route around the failing provider — i.e. failover does
    /// advance, just on the next call rather than mid-stream.
    fn wrap_stream_for_breaker(
        stream: LlmStream,
        breakers: Arc<Mutex<HashMap<String, CircuitBreaker>>>,
        provider_id: String,
        budget_tracker: Arc<BudgetTracker>,
    ) -> LlmStream {
        Box::pin(BreakerStream {
            inner: stream,
            breakers,
            provider_id,
            saw_error: false,
            recorded: false,
            budget_tracker,
            pending_usage: None,
        })
    }

    /// Try each provider in the priority list for the requested profile.
    async fn route_with_failover(&self, request: &LlmRequest) -> Result<LlmResponse> {
        let profile_config = self.profiles.get(&request.profile).ok_or_else(|| {
            AthenError::Config(format!(
                "no profile configuration for {:?}",
                request.profile
            ))
        })?;

        let priority = &profile_config.priority;
        let mut last_error: Option<AthenError> = None;

        for provider_id in priority {
            // Check circuit breaker
            {
                let mut breakers = self.circuit_breakers.lock().unwrap();
                let breaker = breakers.entry(provider_id.clone()).or_default();
                if !breaker.allows_request() {
                    debug!(
                        provider = %provider_id,
                        "circuit breaker open, skipping provider"
                    );
                    continue;
                }
            }

            let provider = match self.providers.get(provider_id) {
                Some(p) => p,
                None => {
                    warn!(provider = %provider_id, "provider not registered, skipping");
                    continue;
                }
            };

            // Retry the SAME provider a bounded number of times on a transient
            // error (429 / 5xx / connection reset) before failover advances.
            // Permanent errors (auth, bad request, unknown model) break out
            // immediately — retrying them only wastes time and money. The
            // circuit breaker records a single failure only after retries are
            // exhausted, so a brief blip that recovers on retry doesn't push the
            // breaker toward open.
            let mut attempt: u32 = 0;
            let provider_outcome = loop {
                attempt += 1;
                match provider.complete(request).await {
                    Ok(response) => break Ok(response),
                    Err(e) => {
                        let retryable = e.is_retryable();
                        if retryable && attempt < MAX_ATTEMPTS_PER_PROVIDER {
                            let retry_after = e.retry_after_secs().map(Duration::from_secs);
                            let delay = backoff_delay(attempt, retry_after, jitter01());
                            warn!(
                                provider = %provider_id,
                                attempt,
                                delay_ms = delay.as_millis() as u64,
                                error = %e,
                                "transient provider error, retrying after backoff"
                            );
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        break Err(e);
                    }
                }
            };

            match provider_outcome {
                Ok(response) => {
                    // Record success in circuit breaker
                    {
                        let mut breakers = self.circuit_breakers.lock().unwrap();
                        if let Some(breaker) = breakers.get_mut(provider_id) {
                            breaker.record_success();
                        }
                    }
                    // Record budget usage
                    self.budget_tracker.record_usage(&response.usage);
                    return Ok(response);
                }
                Err(e) => {
                    warn!(
                        provider = %provider_id,
                        error = %e,
                        "provider call failed (retries exhausted or permanent), trying next"
                    );
                    // Record a single failure in the circuit breaker — after
                    // retries, not per intermediate attempt.
                    {
                        let mut breakers = self.circuit_breakers.lock().unwrap();
                        if let Some(breaker) = breakers.get_mut(provider_id) {
                            breaker.record_failure();
                        }
                    }
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| AthenError::LlmProvider {
            provider: "router".into(),
            message: format!("all providers exhausted for profile {:?}", request.profile),
        }))
    }
}

#[async_trait]
impl LlmRouter for DefaultLlmRouter {
    async fn route(&self, request: &LlmRequest) -> Result<LlmResponse> {
        // Budget check — estimate a minimal cost; actual cost tracked on success.
        if !self.budget_tracker.can_afford(0.0) {
            return Err(AthenError::LlmProvider {
                provider: "router".into(),
                message: "daily budget exhausted".into(),
            });
        }

        self.route_with_failover(request).await
    }

    async fn route_streaming(&self, request: &LlmRequest) -> Result<LlmStream> {
        if !self.budget_tracker.can_afford(0.0) {
            return Err(AthenError::LlmProvider {
                provider: "router".into(),
                message: "daily budget exhausted".into(),
            });
        }

        self.route_streaming_with_failover(request).await
    }

    async fn budget_remaining(&self) -> Result<BudgetStatus> {
        Ok(self.budget_tracker.status())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::llm::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    // ---- Mock provider ----

    struct MockProvider {
        id: String,
        should_fail: bool,
        call_count: Arc<AtomicU32>,
    }

    impl MockProvider {
        fn new(id: &str, should_fail: bool) -> Self {
            Self {
                id: id.to_string(),
                should_fail,
                call_count: Arc::new(AtomicU32::new(0)),
            }
        }

        fn with_counter(id: &str, should_fail: bool, counter: Arc<AtomicU32>) -> Self {
            Self {
                id: id.to_string(),
                should_fail,
                call_count: counter,
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        fn provider_id(&self) -> &str {
            &self.id
        }

        async fn complete(&self, _request: &LlmRequest) -> Result<LlmResponse> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            if self.should_fail {
                Err(AthenError::LlmProvider {
                    provider: self.id.clone(),
                    message: "mock failure".into(),
                })
            } else {
                Ok(LlmResponse {
                    content: format!("response from {}", self.id),
                    reasoning_content: None,
                    model_used: "mock-model".into(),
                    provider: self.id.clone(),
                    usage: TokenUsage {
                        prompt_tokens: 10,
                        completion_tokens: 5,
                        total_tokens: 15,
                        estimated_cost_usd: Some(0.001),
                        ..TokenUsage::default()
                    },
                    tool_calls: vec![],
                    finish_reason: FinishReason::Stop,
                })
            }
        }

        async fn complete_streaming(&self, _request: &LlmRequest) -> Result<LlmStream> {
            Err(AthenError::LlmProvider {
                provider: self.id.clone(),
                message: "streaming not supported in mock".into(),
            })
        }

        async fn is_available(&self) -> bool {
            !self.should_fail
        }
    }

    /// Provider that fails the first `fail_times` calls with a chosen error,
    /// then succeeds. Used to exercise the router retry/backoff path:
    /// `transient = true` yields a retryable `LlmTransient`; `transient = false`
    /// yields a permanent `LlmProvider` that must fail fast (no retry).
    struct FlakyProvider {
        id: String,
        fail_times: u32,
        transient: bool,
        call_count: Arc<AtomicU32>,
    }

    impl FlakyProvider {
        fn new(id: &str, fail_times: u32, transient: bool, counter: Arc<AtomicU32>) -> Self {
            Self {
                id: id.to_string(),
                fail_times,
                transient,
                call_count: counter,
            }
        }
    }

    #[async_trait]
    impl LlmProvider for FlakyProvider {
        fn provider_id(&self) -> &str {
            &self.id
        }

        async fn complete(&self, _request: &LlmRequest) -> Result<LlmResponse> {
            let n = self.call_count.fetch_add(1, Ordering::Relaxed);
            if n < self.fail_times {
                return Err(if self.transient {
                    AthenError::LlmTransient {
                        provider: self.id.clone(),
                        message: "rate limited (mock)".into(),
                        retry_after_secs: None,
                    }
                } else {
                    AthenError::LlmProvider {
                        provider: self.id.clone(),
                        message: "auth error (mock, permanent)".into(),
                    }
                });
            }
            Ok(LlmResponse {
                content: format!("response from {}", self.id),
                reasoning_content: None,
                model_used: "mock-model".into(),
                provider: self.id.clone(),
                usage: TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    estimated_cost_usd: Some(0.001),
                    ..TokenUsage::default()
                },
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
            })
        }

        async fn complete_streaming(&self, _request: &LlmRequest) -> Result<LlmStream> {
            Err(AthenError::LlmProvider {
                provider: self.id.clone(),
                message: "streaming not supported in mock".into(),
            })
        }

        async fn is_available(&self) -> bool {
            true
        }
    }

    /// Provider whose streaming call connects (HTTP-200 equivalent) but then,
    /// depending on `error_mid_stream`, either ends cleanly or yields an `Err`
    /// chunk after some text — i.e. it models a stream that resets mid-flight.
    struct StreamingMockProvider {
        id: String,
        error_mid_stream: bool,
    }

    #[async_trait]
    impl LlmProvider for StreamingMockProvider {
        fn provider_id(&self) -> &str {
            &self.id
        }

        async fn complete(&self, _request: &LlmRequest) -> Result<LlmResponse> {
            Ok(LlmResponse {
                content: format!("nonstream from {}", self.id),
                reasoning_content: None,
                model_used: "mock-model".into(),
                provider: self.id.clone(),
                usage: TokenUsage::default(),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
            })
        }

        async fn complete_streaming(&self, _request: &LlmRequest) -> Result<LlmStream> {
            let id = self.id.clone();
            let error_mid_stream = self.error_mid_stream;
            // Yield one text chunk, then either an error (reset) or a clean end.
            let items: Vec<Result<LlmChunk>> = if error_mid_stream {
                vec![
                    Ok(LlmChunk {
                        delta: "partial ".into(),
                        is_final: false,
                        is_thinking: false,
                        tool_calls: vec![],
                        usage: None,
                    }),
                    Err(AthenError::LlmProvider {
                        provider: id,
                        message: "stream error: connection reset".into(),
                    }),
                ]
            } else {
                vec![
                    Ok(LlmChunk {
                        delta: "complete ".into(),
                        is_final: false,
                        is_thinking: false,
                        tool_calls: vec![],
                        usage: None,
                    }),
                    Ok(LlmChunk {
                        delta: "answer".into(),
                        is_final: true,
                        is_thinking: false,
                        tool_calls: vec![],
                        // Terminal usage-bearing chunk, as a real provider
                        // streaming parser emits.
                        usage: Some(TokenUsage {
                            prompt_tokens: 120,
                            completion_tokens: 48,
                            total_tokens: 168,
                            estimated_cost_usd: Some(0.002),
                            ..TokenUsage::default()
                        }),
                    }),
                ]
            };
            Ok(Box::pin(futures::stream::iter(items)))
        }

        async fn is_available(&self) -> bool {
            true
        }
    }

    fn make_request() -> LlmRequest {
        LlmRequest {
            profile: ModelProfile::Powerful,
            messages: vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
            }],
            max_tokens: Some(100),
            temperature: None,
            tools: None,
            system_prompt: None,
            reasoning_effort: athen_core::llm::ReasoningEffort::default(),
        }
    }

    fn make_profile(priority: Vec<&str>) -> ProfileConfig {
        ProfileConfig {
            description: "test profile".into(),
            priority: priority.into_iter().map(String::from).collect(),
            fallback: None,
        }
    }

    // ---- Failover test ----

    #[tokio::test]
    async fn test_failover_first_fails_second_succeeds() {
        let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
        providers.insert(
            "provider_a".into(),
            Box::new(MockProvider::new("provider_a", true)),
        );
        providers.insert(
            "provider_b".into(),
            Box::new(MockProvider::new("provider_b", false)),
        );

        let mut profiles = HashMap::new();
        profiles.insert(
            ModelProfile::Powerful,
            make_profile(vec!["provider_a", "provider_b"]),
        );

        let router = DefaultLlmRouter::new(providers, profiles, BudgetTracker::new(None));
        let response = router.route(&make_request()).await.unwrap();
        assert_eq!(response.provider, "provider_b");
    }

    #[tokio::test]
    async fn test_all_providers_fail() {
        let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
        providers.insert(
            "provider_a".into(),
            Box::new(MockProvider::new("provider_a", true)),
        );
        providers.insert(
            "provider_b".into(),
            Box::new(MockProvider::new("provider_b", true)),
        );

        let mut profiles = HashMap::new();
        profiles.insert(
            ModelProfile::Powerful,
            make_profile(vec!["provider_a", "provider_b"]),
        );

        let router = DefaultLlmRouter::new(providers, profiles, BudgetTracker::new(None));
        let result = router.route(&make_request()).await;
        assert!(result.is_err());
    }

    // ---- Retry / backoff tests ----

    /// Pure backoff schedule: full-jitter exponential, bounded, with
    /// `Retry-After` taking precedence and capped.
    #[test]
    fn test_backoff_delay_bounded_and_jittered() {
        // rand01 = 0.0 → zero delay; rand01 = 1.0 → the full ceiling.
        assert_eq!(backoff_delay(1, None, 0.0), Duration::from_millis(0));
        assert_eq!(backoff_delay(1, None, 1.0), BACKOFF_BASE); // 500ms ceiling
        assert_eq!(
            backoff_delay(2, None, 1.0),
            Duration::from_millis(BACKOFF_BASE.as_millis() as u64 * 2)
        );

        // Exponential growth is capped at BACKOFF_CAP no matter the attempt.
        assert_eq!(backoff_delay(20, None, 1.0), BACKOFF_CAP);

        // Mid-jitter stays within [0, ceiling].
        let d = backoff_delay(3, None, 0.5);
        assert!(d <= Duration::from_millis(BACKOFF_BASE.as_millis() as u64 * 4));

        // Retry-After wins and is honored (under the cap)...
        assert_eq!(
            backoff_delay(1, Some(Duration::from_secs(5)), 1.0),
            Duration::from_secs(5)
        );
        // ...but a hostile Retry-After is capped.
        assert_eq!(
            backoff_delay(1, Some(Duration::from_secs(99999)), 1.0),
            RETRY_AFTER_CAP
        );
    }

    /// A provider that fails transiently twice then succeeds: the router must
    /// retry the SAME provider and ultimately return success (not failover).
    #[tokio::test]
    async fn test_transient_error_retried_then_succeeds() {
        let counter = Arc::new(AtomicU32::new(0));
        let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
        providers.insert(
            "flaky".into(),
            Box::new(FlakyProvider::new("flaky", 2, true, counter.clone())),
        );
        let mut profiles = HashMap::new();
        profiles.insert(ModelProfile::Powerful, make_profile(vec!["flaky"]));

        let router = DefaultLlmRouter::new(providers, profiles, BudgetTracker::new(None));
        let response = router.route(&make_request()).await.unwrap();
        assert_eq!(response.provider, "flaky");
        // 2 transient failures + 1 success = 3 calls to the same provider.
        assert_eq!(counter.load(Ordering::Relaxed), 3);
        // The breaker saw a SUCCESS (not opened) — retries recovered.
        assert_eq!(router.breaker_state("flaky"), Some(CircuitState::Closed));
    }

    /// A permanent error (e.g. auth) must NOT be retried — exactly one call,
    /// then fail fast.
    #[tokio::test]
    async fn test_permanent_error_not_retried() {
        let counter = Arc::new(AtomicU32::new(0));
        let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
        providers.insert(
            "auth_fail".into(),
            // fail_times huge, but permanent → must not retry at all.
            Box::new(FlakyProvider::new("auth_fail", 99, false, counter.clone())),
        );
        let mut profiles = HashMap::new();
        profiles.insert(ModelProfile::Powerful, make_profile(vec!["auth_fail"]));

        let router = DefaultLlmRouter::new(providers, profiles, BudgetTracker::new(None));
        let result = router.route(&make_request()).await;
        assert!(result.is_err());
        assert!(!result.unwrap_err().is_retryable());
        // Exactly one call — no retry on a permanent error.
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    /// Retries exhausted on the first provider (always-transient) must advance
    /// failover to the next provider.
    #[tokio::test]
    async fn test_retries_exhausted_then_failover_advances() {
        let counter_a = Arc::new(AtomicU32::new(0));
        let counter_b = Arc::new(AtomicU32::new(0));
        let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
        providers.insert(
            "always_429".into(),
            // Always transient-fails → exhausts MAX_ATTEMPTS_PER_PROVIDER.
            Box::new(FlakyProvider::new(
                "always_429",
                u32::MAX,
                true,
                counter_a.clone(),
            )),
        );
        providers.insert(
            "backup".into(),
            Box::new(FlakyProvider::new("backup", 0, true, counter_b.clone())),
        );
        let mut profiles = HashMap::new();
        profiles.insert(
            ModelProfile::Powerful,
            make_profile(vec!["always_429", "backup"]),
        );

        let router = DefaultLlmRouter::new(providers, profiles, BudgetTracker::new(None));
        let response = router.route(&make_request()).await.unwrap();
        assert_eq!(response.provider, "backup");
        // First provider was retried up to the per-provider attempt cap...
        assert_eq!(counter_a.load(Ordering::Relaxed), MAX_ATTEMPTS_PER_PROVIDER);
        // ...then failover advanced to the backup, which succeeded first try.
        assert_eq!(counter_b.load(Ordering::Relaxed), 1);
    }

    // ---- Streaming mid-stream-error tests ----

    /// A stream that yields text then an `Err` chunk must NOT be treated as a
    /// clean success: the breaker records a FAILURE, and after enough such
    /// failures the circuit opens — which is what makes failover advance on the
    /// next (non-streaming fallback or streaming) call.
    #[tokio::test]
    async fn test_streaming_mid_stream_error_records_failure_and_opens_breaker() {
        use futures::StreamExt;

        let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
        providers.insert(
            "stream_a".into(),
            Box::new(StreamingMockProvider {
                id: "stream_a".into(),
                error_mid_stream: true,
            }),
        );
        let mut profiles = HashMap::new();
        profiles.insert(ModelProfile::Powerful, make_profile(vec!["stream_a"]));

        // Low failure threshold so a few mid-stream errors trip the breaker open.
        let mut breakers = HashMap::new();
        breakers.insert(
            "stream_a".to_string(),
            CircuitBreaker::with_thresholds(2, 2, Duration::from_secs(60)),
        );
        let router = DefaultLlmRouter::new(providers, profiles, BudgetTracker::new(None))
            .with_circuit_breakers(breakers);

        // Drive two streams to completion; each yields a partial chunk then errors.
        for _ in 0..2 {
            let mut stream = router.route_streaming(&make_request()).await.unwrap();
            let mut saw_err = false;
            while let Some(item) = stream.next().await {
                if item.is_err() {
                    saw_err = true;
                }
            }
            assert!(saw_err, "mock stream should surface an Err chunk");
        }

        // Two mid-stream failures → breaker is OPEN. A connection-time success
        // (the old bug) would have left it Closed forever.
        assert_eq!(
            router.breaker_state("stream_a"),
            Some(CircuitState::Open),
            "mid-stream errors must count as breaker failures so failover advances"
        );
    }

    /// A cleanly-terminating stream records a SUCCESS — the genuine success path
    /// is preserved exactly.
    #[tokio::test]
    async fn test_streaming_clean_end_records_success() {
        use futures::StreamExt;

        let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
        providers.insert(
            "stream_ok".into(),
            Box::new(StreamingMockProvider {
                id: "stream_ok".into(),
                error_mid_stream: false,
            }),
        );
        let mut profiles = HashMap::new();
        profiles.insert(ModelProfile::Powerful, make_profile(vec!["stream_ok"]));

        // Start in half-open: a recorded success should close the breaker.
        let mut breakers = HashMap::new();
        let mut cb = CircuitBreaker::with_thresholds(2, 1, Duration::from_millis(1));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
        std::thread::sleep(Duration::from_millis(5));
        assert!(cb.allows_request()); // -> HalfOpen
        assert_eq!(cb.state(), CircuitState::HalfOpen);
        breakers.insert("stream_ok".to_string(), cb);

        let router = DefaultLlmRouter::new(providers, profiles, BudgetTracker::new(None))
            .with_circuit_breakers(breakers);

        let mut stream = router.route_streaming(&make_request()).await.unwrap();
        let mut collected = String::new();
        while let Some(item) = stream.next().await {
            if let Ok(chunk) = item {
                collected.push_str(&chunk.delta);
            }
        }
        assert_eq!(collected, "complete answer");

        // Clean end recorded success (success_threshold 1) → breaker closed.
        assert_eq!(
            router.breaker_state("stream_ok"),
            Some(CircuitState::Closed),
            "a clean stream end must record success and recover the breaker"
        );
    }

    /// A clean streamed turn must decrement the budget by the usage carried on
    /// the terminal chunk — the regression this whole change fixes (streamed
    /// turns previously reported zero usage, so the budget never moved).
    #[tokio::test]
    async fn test_streaming_clean_end_records_usage_against_budget() {
        use futures::StreamExt;

        let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
        providers.insert(
            "stream_ok".into(),
            Box::new(StreamingMockProvider {
                id: "stream_ok".into(),
                error_mid_stream: false,
            }),
        );
        let mut profiles = HashMap::new();
        profiles.insert(ModelProfile::Powerful, make_profile(vec!["stream_ok"]));

        let router = DefaultLlmRouter::new(providers, profiles, BudgetTracker::new(Some(10.0)));

        // Before consuming: nothing spent.
        let before = router.budget_remaining().await.unwrap();
        assert_eq!(before.tokens_used_today, 0);
        assert!((before.spent_today_usd).abs() < f64::EPSILON);

        // Fully drain the stream (clean end).
        let mut stream = router.route_streaming(&make_request()).await.unwrap();
        while stream.next().await.is_some() {}

        // The terminal chunk's usage (168 tokens / $0.002) must be recorded.
        let after = router.budget_remaining().await.unwrap();
        assert_eq!(
            after.tokens_used_today, 168,
            "streamed turn must record its token usage"
        );
        assert!(
            (after.spent_today_usd - 0.002).abs() < 1e-9,
            "streamed turn must record its cost, got {}",
            after.spent_today_usd
        );
    }

    /// A mid-stream error must NOT record usage — the executor falls back to a
    /// non-streaming `route()` that records its own spend, so recording the
    /// failed stream too would double-count.
    #[tokio::test]
    async fn test_streaming_mid_stream_error_records_no_usage() {
        use futures::StreamExt;

        let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
        providers.insert(
            "stream_err".into(),
            Box::new(StreamingMockProvider {
                id: "stream_err".into(),
                error_mid_stream: true,
            }),
        );
        let mut profiles = HashMap::new();
        profiles.insert(ModelProfile::Powerful, make_profile(vec!["stream_err"]));

        let router = DefaultLlmRouter::new(providers, profiles, BudgetTracker::new(Some(10.0)));

        let mut stream = router.route_streaming(&make_request()).await.unwrap();
        while stream.next().await.is_some() {}

        let after = router.budget_remaining().await.unwrap();
        assert_eq!(
            after.tokens_used_today, 0,
            "a failed stream must not record usage (fallback owns the spend)"
        );
    }

    // ---- Circuit breaker tests ----

    #[test]
    fn test_circuit_breaker_state_transitions() {
        let mut cb = CircuitBreaker::with_thresholds(3, 2, Duration::from_millis(50));

        // Starts closed
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.allows_request());

        // Fail 3 times → opens
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Closed);
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);

        // While open, requests are blocked
        assert!(!cb.allows_request());
    }

    #[test]
    fn test_circuit_breaker_half_open_recovery() {
        let mut cb = CircuitBreaker::with_thresholds(
            2,
            2,
            Duration::from_millis(1), // very short timeout for tests
        );

        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);

        // Wait for timeout
        std::thread::sleep(Duration::from_millis(10));

        // Should transition to half-open
        assert!(cb.allows_request());
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // Success in half-open
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::HalfOpen); // need 2 successes
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed); // recovered
    }

    #[test]
    fn test_circuit_breaker_half_open_failure_reopens() {
        let mut cb = CircuitBreaker::with_thresholds(2, 2, Duration::from_millis(1));

        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);

        std::thread::sleep(Duration::from_millis(10));
        assert!(cb.allows_request());
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // Failure in half-open → back to open
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
    }

    #[tokio::test]
    async fn test_router_skips_circuit_broken_provider() {
        let counter_a = Arc::new(AtomicU32::new(0));
        let counter_b = Arc::new(AtomicU32::new(0));

        let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
        providers.insert(
            "provider_a".into(),
            Box::new(MockProvider::with_counter(
                "provider_a",
                true,
                counter_a.clone(),
            )),
        );
        providers.insert(
            "provider_b".into(),
            Box::new(MockProvider::with_counter(
                "provider_b",
                false,
                counter_b.clone(),
            )),
        );

        let mut profiles = HashMap::new();
        profiles.insert(
            ModelProfile::Powerful,
            make_profile(vec!["provider_a", "provider_b"]),
        );

        // Set up provider_a with an already-open circuit breaker
        let mut breakers = HashMap::new();
        let mut cb = CircuitBreaker::with_thresholds(2, 2, Duration::from_secs(600));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
        breakers.insert("provider_a".into(), cb);
        breakers.insert("provider_b".into(), CircuitBreaker::new());

        let router = DefaultLlmRouter::new(providers, profiles, BudgetTracker::new(None))
            .with_circuit_breakers(breakers);

        let response = router.route(&make_request()).await.unwrap();
        assert_eq!(response.provider, "provider_b");
        // provider_a should not have been called (circuit was open)
        assert_eq!(counter_a.load(Ordering::Relaxed), 0);
        assert_eq!(counter_b.load(Ordering::Relaxed), 1);
    }

    // ---- Budget enforcement tests ----

    #[tokio::test]
    async fn test_budget_enforcement_rejects_when_exhausted() {
        let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
        providers.insert(
            "provider_a".into(),
            Box::new(MockProvider::new("provider_a", false)),
        );

        let mut profiles = HashMap::new();
        profiles.insert(ModelProfile::Powerful, make_profile(vec!["provider_a"]));

        let tracker = BudgetTracker::new(Some(0.0)); // zero budget

        let router = DefaultLlmRouter::new(providers, profiles, tracker);
        let result = router.route(&make_request()).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("budget"));
    }

    #[tokio::test]
    async fn test_budget_tracked_after_successful_call() {
        let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
        providers.insert(
            "provider_a".into(),
            Box::new(MockProvider::new("provider_a", false)),
        );

        let mut profiles = HashMap::new();
        profiles.insert(ModelProfile::Powerful, make_profile(vec!["provider_a"]));

        let tracker = BudgetTracker::new(Some(1.0));
        let router = DefaultLlmRouter::new(providers, profiles, tracker);

        let _response = router.route(&make_request()).await.unwrap();

        let status = router.budget_remaining().await.unwrap();
        assert!(status.spent_today_usd > 0.0);
        assert!(status.tokens_used_today > 0);
    }
}
