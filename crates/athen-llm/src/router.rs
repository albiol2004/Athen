//! Profile-based routing with failover and budget enforcement.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tracing::{debug, warn};

use athen_core::config::ProfileConfig;
use athen_core::error::{AthenError, Result};
use athen_core::llm::{BudgetStatus, LlmRequest, LlmResponse, LlmStream, ModelProfile};
use athen_core::traits::llm::{LlmProvider, LlmRouter};

use crate::budget::BudgetTracker;

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
// Router
// ---------------------------------------------------------------------------

/// Default implementation of [`LlmRouter`] with profile-based routing,
/// failover chains, circuit breakers, and budget enforcement.
pub struct DefaultLlmRouter {
    providers: HashMap<String, Box<dyn LlmProvider>>,
    profiles: HashMap<ModelProfile, ProfileConfig>,
    budget_tracker: BudgetTracker,
    circuit_breakers: Mutex<HashMap<String, CircuitBreaker>>,
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
            budget_tracker,
            circuit_breakers: Mutex::new(breaker_map),
        }
    }

    /// Build a router with custom circuit breaker settings per provider.
    pub fn with_circuit_breakers(self, breakers: HashMap<String, CircuitBreaker>) -> Self {
        *self.circuit_breakers.lock().unwrap() = breakers;
        self
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

            match provider.complete_streaming(request).await {
                Ok(stream) => {
                    // Record success — note: for streaming we record success
                    // at connection time. Individual chunk errors are handled
                    // by the consumer of the stream.
                    {
                        let mut breakers = self.circuit_breakers.lock().unwrap();
                        if let Some(breaker) = breakers.get_mut(provider_id) {
                            breaker.record_success();
                        }
                    }
                    return Ok(stream);
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

            match provider.complete(request).await {
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
                        "provider call failed, trying next"
                    );
                    // Record failure in circuit breaker
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
