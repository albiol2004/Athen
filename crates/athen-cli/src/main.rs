use std::collections::HashMap;
use std::io::{BufRead, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use uuid::Uuid;

use athen_core::config::{AuthType, ProfileConfig};
use athen_core::config_loader;
use athen_core::error::Result;
use athen_core::event::{EventKind, EventSource, NormalizedContent, SenseEvent};
use athen_core::llm::{
    BudgetStatus, LlmRequest, LlmResponse, ModelProfile,
};
use athen_core::risk::RiskLevel;
use athen_core::task::{DomainType, Task, TaskPriority, TaskStatus};
use athen_core::traits::agent::AgentExecutor;
use athen_core::traits::coordinator::TaskQueue;
use athen_core::traits::llm::LlmRouter;
use athen_agent::{AgentBuilder, ShellToolRegistry};
use athen_coordinador::Coordinator;
use athen_llm::budget::BudgetTracker;
use athen_llm::providers::deepseek::DeepSeekProvider;
use athen_llm::router::DefaultLlmRouter;
use athen_risk::llm_fallback::LlmRiskEvaluator;
use athen_risk::CombinedRiskEvaluator;

// ---------------------------------------------------------------------------
// Shared router wrapper
// ---------------------------------------------------------------------------

struct SharedRouter(Arc<DefaultLlmRouter>);

#[async_trait]
impl LlmRouter for SharedRouter {
    async fn route(&self, request: &LlmRequest) -> Result<LlmResponse> {
        self.0.route(request).await
    }
    async fn budget_remaining(&self) -> Result<BudgetStatus> {
        self.0.budget_remaining().await
    }
}

// ---------------------------------------------------------------------------
// Event construction
// ---------------------------------------------------------------------------

fn make_event(message: &str) -> SenseEvent {
    SenseEvent {
        id: Uuid::new_v4(),
        timestamp: Utc::now(),
        source: EventSource::UserInput,
        kind: EventKind::Command,
        sender: None,
        content: NormalizedContent {
            summary: Some(message.to_string()),
            body: serde_json::Value::String(message.to_string()),
            attachments: Vec::new(),
        },
        source_risk: RiskLevel::Safe,
        raw_id: None,
    }
}

// ---------------------------------------------------------------------------
// Synchronous stdin reader — returns None on EOF or error
// ---------------------------------------------------------------------------

fn read_line_sync(prompt: &str) -> Option<String> {
    print!("{prompt}");
    std::io::stdout().flush().ok();

    let stdin = std::io::stdin();
    let mut line = String::new();
    match stdin.lock().read_line(&mut line) {
        Ok(0) => None, // EOF
        Ok(_) => Some(line),
        Err(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

/// Resolve the config path, trying in order:
/// 1. `~/.athen/config.toml`
/// 2. `./config/config.toml` (project-local fallback)
///
/// Returns the directory path if a config file is found, or None for defaults.
fn find_config_dir() -> Option<PathBuf> {
    // Try ~/.athen/
    if let Some(home) = std::env::var_os("HOME") {
        let home_config = PathBuf::from(home).join(".athen");
        if home_config.join("config.toml").exists() {
            return Some(home_config);
        }
    }

    // Try ./config/
    let local_config = PathBuf::from("config");
    if local_config.join("config.toml").exists() {
        return Some(local_config);
    }

    None
}

// ---------------------------------------------------------------------------
// System initialisation
// ---------------------------------------------------------------------------

fn build_router(api_key: String) -> Arc<DefaultLlmRouter> {
    let provider = DeepSeekProvider::new(api_key);

    let mut providers: HashMap<String, Box<dyn athen_core::traits::llm::LlmProvider>> =
        HashMap::new();
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

    let budget = BudgetTracker::new(None);

    Arc::new(DefaultLlmRouter::new(providers, profiles, budget))
}

fn build_coordinator(router: &Arc<DefaultLlmRouter>) -> Coordinator {
    let risk_router: Box<dyn LlmRouter> = Box::new(SharedRouter(Arc::clone(router)));
    let llm_evaluator = LlmRiskEvaluator::new(risk_router);
    let combined = CombinedRiskEvaluator::new(llm_evaluator);
    Coordinator::new(Box::new(combined))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Minimal tracing — only warnings, no noisy info/debug.
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_target(false)
        .compact()
        .init();

    // Load configuration.
    let config = match find_config_dir() {
        Some(dir) => {
            eprintln!("Loading config from: {}", dir.display());
            match config_loader::load_config_dir(&dir) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error loading config: {e}");
                    eprintln!("Falling back to defaults.");
                    athen_core::config::AthenConfig::default()
                }
            }
        }
        None => {
            eprintln!("No config file found, using defaults.");
            athen_core::config::AthenConfig::default()
        }
    };

    // Read API key: env var takes precedence, then config file.
    let api_key = match std::env::var("DEEPSEEK_API_KEY") {
        Ok(key) if !key.is_empty() => key,
        _ => {
            // Try to get from config providers
            match config.models.providers.get("deepseek") {
                Some(provider) => match &provider.auth {
                    AuthType::ApiKey(key)
                        if !key.is_empty() && !key.starts_with("${") =>
                    {
                        key.clone()
                    }
                    _ => {
                        eprintln!("Error: DEEPSEEK_API_KEY environment variable not set.");
                        eprintln!(
                            "Export it before running:  export DEEPSEEK_API_KEY=\"sk-...\""
                        );
                        std::process::exit(1);
                    }
                },
                None => {
                    eprintln!("Error: DEEPSEEK_API_KEY environment variable not set.");
                    eprintln!(
                        "Export it before running:  export DEEPSEEK_API_KEY=\"sk-...\""
                    );
                    std::process::exit(1);
                }
            }
        }
    };

    // Build the system.
    let router = build_router(api_key);
    let coordinator = build_coordinator(&router);

    // Register one agent.
    let agent_id = Uuid::new_v4();
    coordinator.dispatcher().register_agent(agent_id).await;

    println!("Athen v0.1.0 — Universal AI Agent");
    println!("Type your message (Ctrl+D to exit)");
    println!();

    loop {
        // Read input synchronously — clean EOF/Ctrl+D handling.
        let line = match read_line_sync("> ") {
            Some(l) => l,
            None => {
                // EOF — clean exit.
                println!();
                println!("Goodbye!");
                break;
            }
        };

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if line == "/quit" || line == "/exit" {
            println!("Goodbye!");
            break;
        }

        // Process through coordinator pipeline.
        let event = make_event(line);
        let task_results = match coordinator.process_event(event).await {
            Ok(ids) => ids,
            Err(e) => {
                eprintln!("  Error: {e}");
                continue;
            }
        };

        if task_results.is_empty() {
            eprintln!("  No tasks created.");
            continue;
        }

        // Helper: run a task through the full agent executor.
        let run_task = |description: String, router: Arc<DefaultLlmRouter>| async move {
            let exec_router: Box<dyn LlmRouter> = Box::new(SharedRouter(router));
            let registry = ShellToolRegistry::new().await;

            let executor = AgentBuilder::new()
                .llm_router(exec_router)
                .tool_registry(Box::new(registry))
                .max_steps(20)
                .timeout(Duration::from_secs(120))
                .build()?;

            let task = Task {
                id: Uuid::new_v4(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                source_event: None,
                domain: DomainType::Base,
                description,
                priority: TaskPriority::Normal,
                status: TaskStatus::InProgress,
                risk_score: None,
                risk_budget: None,
                risk_used: 0,
                assigned_agent: None,
                steps: vec![],
                deadline: None,
            };

            executor.execute(task).await
        };

        // Try to dispatch.
        match coordinator.dispatch_next().await {
            Ok(Some((task_id, _))) => {
                // Task approved and dispatched.
                println!("  [approved] Thinking...");

                match run_task(line.to_string(), Arc::clone(&router)).await {
                    Ok(result) => {
                        println!();
                        if let Some(output) = &result.output {
                            if let Some(response) = output.get("response").and_then(|r| r.as_str())
                            {
                                println!("{response}");
                            } else {
                                println!("{}", serde_json::to_string_pretty(output).unwrap_or_default());
                            }
                        }
                        if !result.success {
                            eprintln!("  (task ended without full completion)");
                        }
                        println!();
                    }
                    Err(e) => {
                        eprintln!("  Agent error: {e}");
                        println!();
                    }
                }

                // Release agent.
                let _ = coordinator.complete_task(task_id).await;
            }
            Ok(None) => {
                // Blocked by risk system.
                let pending = coordinator.queue().pending_count().await.unwrap_or(0);
                if pending > 0 {
                    eprintln!("  No agents available.");
                } else {
                    println!("  [blocked] Risk too high. Approve? (y/n)");
                    if let Some(answer) = read_line_sync("  > ") {
                        if answer.trim().eq_ignore_ascii_case("y") {
                            println!("  Thinking...");
                            match run_task(line.to_string(), Arc::clone(&router)).await {
                                Ok(result) => {
                                    println!();
                                    if let Some(output) = &result.output {
                                        if let Some(response) =
                                            output.get("response").and_then(|r| r.as_str())
                                        {
                                            println!("{response}");
                                        } else {
                                            println!(
                                                "{}",
                                                serde_json::to_string_pretty(output)
                                                    .unwrap_or_default()
                                            );
                                        }
                                    }
                                    println!();
                                }
                                Err(e) => {
                                    eprintln!("  Agent error: {e}");
                                    println!();
                                }
                            }
                        } else {
                            println!("  Blocked.");
                            println!();
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("  Dispatch error: {e}");
            }
        }
    }
}
