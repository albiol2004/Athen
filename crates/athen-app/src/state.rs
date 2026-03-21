//! Application state management.
//!
//! Builds the coordinator, LLM router, and risk evaluator, wiring them
//! together as the composition root for the Athen desktop app.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use athen_core::config::ProfileConfig;
use athen_core::error::Result;
use athen_core::llm::*;
use athen_core::traits::llm::{LlmProvider, LlmRouter};
use athen_coordinador::Coordinator;
use athen_llm::budget::BudgetTracker;
use athen_llm::providers::deepseek::DeepSeekProvider;
use athen_llm::router::DefaultLlmRouter;
use athen_risk::llm_fallback::LlmRiskEvaluator;
use athen_risk::CombinedRiskEvaluator;

/// Wrapper to share the router via `Arc` while satisfying the `LlmRouter` trait.
pub(crate) struct SharedRouter(pub Arc<DefaultLlmRouter>);

#[async_trait]
impl LlmRouter for SharedRouter {
    async fn route(&self, request: &LlmRequest) -> Result<LlmResponse> {
        self.0.route(request).await
    }
    async fn budget_remaining(&self) -> Result<BudgetStatus> {
        self.0.budget_remaining().await
    }
}

/// Top-level application state managed by Tauri.
pub struct AppState {
    pub coordinator: Coordinator,
    pub router: Arc<DefaultLlmRouter>,
}

impl AppState {
    /// Create a new `AppState`, reading configuration from environment variables.
    ///
    /// Currently wires a single DeepSeek provider. Provider API key is read
    /// from the `DEEPSEEK_API_KEY` environment variable (empty string if unset).
    pub fn new() -> Self {
        let api_key = std::env::var("DEEPSEEK_API_KEY").unwrap_or_default();

        let router = build_router(api_key);
        let coordinator = build_coordinator(&router);

        Self {
            coordinator,
            router,
        }
    }
}

/// Build the LLM router with the DeepSeek provider and default profiles.
fn build_router(api_key: String) -> Arc<DefaultLlmRouter> {
    let provider = DeepSeekProvider::new(api_key);

    let mut providers: HashMap<String, Box<dyn LlmProvider>> = HashMap::new();
    providers.insert("deepseek".into(), Box::new(provider));

    let profile = ProfileConfig {
        description: "DeepSeek default".into(),
        priority: vec!["deepseek".into()],
        fallback: None,
    };

    let mut profiles = HashMap::new();
    profiles.insert(ModelProfile::Powerful, profile.clone());
    profiles.insert(ModelProfile::Fast, profile.clone());
    profiles.insert(ModelProfile::Code, profile.clone());
    profiles.insert(ModelProfile::Cheap, profile);

    Arc::new(DefaultLlmRouter::new(
        providers,
        profiles,
        BudgetTracker::new(None),
    ))
}

/// Build the coordinator with the combined (rules + LLM) risk evaluator.
fn build_coordinator(router: &Arc<DefaultLlmRouter>) -> Coordinator {
    let risk_router: Box<dyn LlmRouter> = Box::new(SharedRouter(Arc::clone(router)));
    let llm_evaluator = LlmRiskEvaluator::new(risk_router);
    let combined = CombinedRiskEvaluator::new(llm_evaluator);
    Coordinator::new(Box::new(combined))
}
