use std::collections::HashMap;
use std::io::{BufRead, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use uuid::Uuid;

use athen_agent::{AgentBuilder, ShellToolRegistry};
use athen_coordinador::Coordinator;
use athen_core::config::{AuthType, ProfileConfig};
use athen_core::config_loader;
use athen_core::error::Result;
use athen_core::event::{EventKind, EventSource, NormalizedContent, SenseEvent};
use athen_core::llm::{BudgetStatus, LlmRequest, LlmResponse, ModelFamily, ModelProfile};
use athen_core::risk::RiskLevel;
use athen_core::task::{DomainType, Task, TaskPriority, TaskStatus};
use athen_core::traits::agent::AgentExecutor;
use athen_core::traits::coordinator::TaskQueue;
use athen_core::traits::llm::LlmRouter;
use athen_llm::budget::BudgetTracker;
use athen_llm::providers::deepseek::DeepSeekProvider;
use athen_llm::providers::openai::OpenAiCompatibleProvider;
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
    // Try Athen's data dir (~/.athen on Unix, %APPDATA%\Athen on Windows).
    if let Some(data_dir) = athen_core::paths::athen_data_dir() {
        if data_dir.join("config.toml").exists() {
            return Some(data_dir);
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
// Headless one-shot mode (benchmark harness driver)
// ---------------------------------------------------------------------------

/// Build a router backed by a single provider. DeepSeek-family selections
/// route through the dedicated `DeepSeekProvider` (which knows the wire
/// quirks: V4 Flash thinking-disable, reasoning_content echo, etc.); all
/// other families go through the generic `OpenAiCompatibleProvider`.
fn build_openai_compat_router(
    base_url: String,
    model: String,
    api_key: Option<String>,
    family: ModelFamily,
) -> Arc<DefaultLlmRouter> {
    let is_deepseek_family = matches!(
        family,
        ModelFamily::DeepSeekV4Chat | ModelFamily::DeepSeekV4Pro | ModelFamily::DeepSeekR1
    );

    let (provider_id, provider): (String, Box<dyn athen_core::traits::llm::LlmProvider>) =
        if is_deepseek_family {
            // DeepSeek provider hits `{base_url}/v1/chat/completions`, so the
            // caller's base URL must be the host root (e.g. `https://api.deepseek.com`).
            // If the caller passed a `/v1` suffix (the OpenAI-compat convention),
            // strip it so we don't double up.
            let host_root = base_url
                .strip_suffix("/v1")
                .or_else(|| base_url.strip_suffix("/v1/"))
                .unwrap_or(&base_url)
                .to_string();
            let key = api_key.unwrap_or_default();
            let provider = DeepSeekProvider::new(key)
                .with_base_url(host_root)
                .with_model(model)
                .with_family(family);
            ("deepseek".to_string(), Box::new(provider))
        } else {
            let pid = "openai-compat".to_string();
            let mut provider = OpenAiCompatibleProvider::new(base_url)
                .with_model(model)
                .with_provider_id(pid.clone())
                .with_family(family);
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            (pid, Box::new(provider))
        };

    let mut providers: HashMap<String, Box<dyn athen_core::traits::llm::LlmProvider>> =
        HashMap::new();
    providers.insert(provider_id.clone(), provider);

    let profile = ProfileConfig {
        description: "OpenAI-compatible local backend".into(),
        priority: vec![provider_id],
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

fn print_usage() {
    println!("Athen CLI — Universal AI Agent");
    println!();
    println!("USAGE:");
    println!(
        "    athen-cli                                  Launch interactive REPL (uses DeepSeek)"
    );
    println!(
        "    athen-cli --prompt <PROMPT>                Headless one-shot mode (OpenAI-compatible)"
    );
    println!(
        "    athen-cli --profile <ID> --prompt <STR>    Headless mode with a seeded agent profile"
    );
    println!("    athen-cli --help                           Show this help");
    println!();
    println!("FLAGS:");
    println!("    --prompt <STR>     Prompt for headless one-shot execution.");
    println!("    --profile <ID>     Activate a seeded AgentProfile (e.g. 'coder', 'researcher')");
    println!("                       on the executor. Ignored when --prompt is absent.");
    println!(
        "    --family <ID>      Model-family wire ID for per-model quirks (e.g. 'Qwen35Local',"
    );
    println!("                       'DeepSeekR1', 'Llama4Instruct'). Overrides ATHEN_FAMILY.");
    println!(
        "                       Defaults to 'Default' (baseline structured-tool-call behavior)."
    );
    println!("    --temperature <F>  Sampling temperature for the main agent loop (e.g. 0.2 for");
    println!("                       benchmark determinism, 0.7 default, 1.0+ for exploration).");
    println!("                       Overrides ATHEN_TEMPERATURE. Not clamped.");
    println!("    --max-steps <N>    Per-task iteration cap (default 50). Use 0 for unlimited.");
    println!("                       The 600s timeout still applies. Overrides ATHEN_MAX_STEPS.");
    println!();
    println!("HEADLESS MODE ENV VARS:");
    println!("    ATHEN_BASE_URL    (required)  e.g. http://localhost:8000/v1");
    println!("    ATHEN_MODEL       (required)  e.g. Qwen3.5-9B");
    println!("    ATHEN_FAMILY      (optional)  e.g. Qwen35Local — see --family.");
    println!("    ATHEN_TEMPERATURE (optional)  e.g. 0.2 — see --temperature.");
    println!("    ATHEN_API_KEY     (optional)  Bearer token, if backend needs one");
    println!();
    println!("In headless mode the agent runs against the current working directory,");
    println!("auto-approves all actions (no risk gating), prints the final response to");
    println!("stdout, and exits 0 on success or non-zero on error.");
}

/// Open Athen's SQLite database (creating it if needed) and resolve the
/// requested profile id into a `ResolvedAgentProfile`. On unknown id,
/// returns an error message that lists the valid profile ids and exits
/// with code 2 (caller's responsibility).
///
/// Built-in profiles seed automatically on connection (`Database::new`
/// runs migrations + `seed_builtins_if_empty`), so a fresh install will
/// still resolve `coder`, `researcher`, etc. without user intervention.
async fn load_resolved_profile(
    profile_id: &str,
) -> std::result::Result<athen_core::agent_profile::ResolvedAgentProfile, (i32, String)> {
    use athen_core::traits::profile::ProfileStore;

    // Resolve DB path: <athen_data_dir>/athen.db. Create the directory if
    // needed so first runs against a clean home don't fail.
    let data_dir = athen_core::paths::athen_data_dir().ok_or((
        1,
        "Error: cannot resolve Athen data directory (no $HOME).".to_string(),
    ))?;
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        return Err((1, format!("Failed to create {}: {e}", data_dir.display())));
    }
    let db_path = data_dir.join("athen.db");
    let db = athen_persistence::Database::new(&db_path)
        .await
        .map_err(|e| {
            (
                1,
                format!("Failed to open DB at {}: {e}", db_path.display()),
            )
        })?;

    let store = db.profile_store();

    let profile = match store.get_profile(profile_id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            // Unknown id — list the valid built-ins for the user.
            let valid: Vec<String> = match store.list_profiles().await {
                Ok(profiles) => profiles.into_iter().map(|p| p.id).collect(),
                Err(_) => Vec::new(),
            };
            let listing = if valid.is_empty() {
                "(unable to list valid profiles)".to_string()
            } else {
                valid.join(", ")
            };
            return Err((
                2,
                format!("Error: unknown profile id '{profile_id}'.\nValid profile ids: {listing}"),
            ));
        }
        Err(e) => return Err((1, format!("Profile lookup failed: {e}"))),
    };

    let templates = store
        .resolve_templates(&profile.persona_template_ids)
        .await
        .unwrap_or_default();

    Ok(athen_core::agent_profile::ResolvedAgentProfile {
        profile,
        persona_templates: templates,
    })
}

/// Run the agent in headless one-shot mode for benchmark harnesses.
///
/// When `profile_id` is `Some`, the corresponding seeded `AgentProfile`
/// is loaded and activated on the executor (e.g. "coder" swaps the
/// universal Athen persona for the senior-software-engineer prompt).
/// When `None`, behavior is unchanged: the universal persona runs.
async fn run_headless(
    prompt: String,
    profile_id: Option<String>,
    family: ModelFamily,
    temperature: Option<f32>,
    max_steps: u32,
) -> std::result::Result<(), (i32, String)> {
    // 1. Read required env vars.
    let base_url = match std::env::var("ATHEN_BASE_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            return Err((
                2,
                "Error: ATHEN_BASE_URL not set (required for headless mode).".into(),
            ));
        }
    };
    let model = match std::env::var("ATHEN_MODEL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            return Err((
                2,
                "Error: ATHEN_MODEL not set (required for headless mode).".into(),
            ));
        }
    };
    let api_key = std::env::var("ATHEN_API_KEY")
        .ok()
        .filter(|s| !s.is_empty());

    // 2. Optionally resolve the agent profile BEFORE building the executor.
    //    Doing this first means an unknown id fails fast, before we touch
    //    the LLM backend.
    let resolved_profile = match profile_id.as_deref() {
        Some(id) => Some(load_resolved_profile(id).await?),
        None => None,
    };

    // 3. Build router (no coordinator, no risk gating — auto-approve).
    let router = build_openai_compat_router(base_url, model, api_key, family);

    // 4. Build executor. cwd is implicit — the agent's filesystem tools operate
    //    on the harness's working directory.
    let exec_router: Box<dyn LlmRouter> = Box::new(SharedRouter(Arc::clone(&router)));
    let registry = ShellToolRegistry::new().await;

    let mut builder = AgentBuilder::new()
        .llm_router(exec_router)
        .tool_registry(Box::new(registry))
        .max_steps(max_steps)
        .timeout(Duration::from_secs(600))
        .default_temperature(temperature);
    if let Some(rp) = resolved_profile {
        builder = builder.active_profile(rp);
    }
    let executor = builder
        .build()
        .map_err(|e| (1, format!("Failed to build executor: {e}")))?;

    let task = Task {
        id: Uuid::new_v4(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        source_event: None,
        domain: DomainType::Base,
        description: prompt,
        priority: TaskPriority::Normal,
        status: TaskStatus::InProgress,
        risk_score: None,
        risk_budget: None,
        risk_used: 0,
        assigned_agent: None,
        steps: vec![],
        deadline: None,
    };

    // 4. Execute.
    let result = executor
        .execute(task)
        .await
        .map_err(|e| (1, format!("Agent execution error: {e}")))?;

    // 5. Print final response to stdout.
    if let Some(output) = &result.output {
        if let Some(response) = output.get("response").and_then(|r| r.as_str()) {
            println!("{response}");
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(output).unwrap_or_default()
            );
        }
    }

    if !result.success {
        return Err((1, "Task ended without full completion.".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Tracing: honor `RUST_LOG` when set; otherwise default to WARN to keep
    // the headless run quiet for benchmark harnesses. Examples:
    //   RUST_LOG=athen_llm=debug,athen_agent=info  athen-cli --prompt ...
    //   RUST_LOG=debug                              (everything, very noisy)
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .compact()
        .init();

    // Argv dispatch:
    //   --help                         → usage
    //   --prompt <STR>                 → headless, universal persona
    //   --profile <ID> --prompt <STR>  → headless, profile-specific persona
    //   --prompt <STR> --profile <ID>  → same, either flag order
    //   (default)                      → REPL
    //
    // `--profile` without `--prompt` is silently ignored (REPL stays as-is).
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return;
    }

    // Generic flag-pair extractor: returns the value that follows the named
    // flag, or `None` if the flag is absent. Returns `Err` with an error
    // message if the flag is present but missing/empty value.
    fn extract_flag(args: &[String], flag: &str) -> std::result::Result<Option<String>, String> {
        match args.iter().position(|a| a == flag) {
            Some(idx) => match args.get(idx + 1) {
                Some(v) if !v.is_empty() => Ok(Some(v.clone())),
                _ => Err(format!("{flag} requires a non-empty argument.")),
            },
            None => Ok(None),
        }
    }

    let prompt_arg = match extract_flag(&args, "--prompt") {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("Error: {msg}");
            eprintln!("Usage: athen-cli --prompt <PROMPT> [--profile <ID>]");
            std::process::exit(2);
        }
    };
    let profile_arg = match extract_flag(&args, "--profile") {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("Error: {msg}");
            eprintln!("Usage: athen-cli --prompt <PROMPT> [--profile <ID>] [--family <ID>]");
            std::process::exit(2);
        }
    };
    // Family selection: `--family <ID>` overrides `ATHEN_FAMILY`, which
    // overrides `Default`. Unknown wire IDs hard-error so a benchmark run
    // can't silently fall back to baseline behavior and look like a quirks
    // regression.
    let family_arg = match extract_flag(&args, "--family") {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("Error: {msg}");
            eprintln!("Usage: athen-cli --prompt <PROMPT> [--profile <ID>] [--family <ID>]");
            std::process::exit(2);
        }
    };
    let family_str =
        family_arg.or_else(|| std::env::var("ATHEN_FAMILY").ok().filter(|s| !s.is_empty()));
    let family = match family_str.as_deref() {
        None => ModelFamily::Default,
        Some(s) => match ModelFamily::from_wire_id(s) {
            Some(f) => f,
            None => {
                eprintln!("Error: unknown model family '{s}'.");
                eprintln!(
                    "Known wire IDs: {}",
                    ModelFamily::all()
                        .iter()
                        .map(|f| f.wire_id())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                std::process::exit(2);
            }
        },
    };

    // Sampling temperature for the main agent loop. `--temperature` overrides
    // `ATHEN_TEMPERATURE`, which falls through to the executor default (0.7).
    // No clamping — pass through to the provider so backend-side errors
    // surface verbatim. Lower values (0.0–0.3) = more reproducible benchmark
    // runs and tighter tool-call discipline; higher = more exploration.
    let temperature_arg = match extract_flag(&args, "--temperature") {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("Error: {msg}");
            eprintln!(
                "Usage: athen-cli --prompt <PROMPT> [--profile <ID>] [--family <ID>] [--temperature <FLOAT>]"
            );
            std::process::exit(2);
        }
    };
    let temperature_str = temperature_arg.or_else(|| {
        std::env::var("ATHEN_TEMPERATURE")
            .ok()
            .filter(|s| !s.is_empty())
    });
    let temperature = match temperature_str.as_deref() {
        None => None,
        Some(s) => match s.parse::<f32>() {
            Ok(t) => Some(t),
            Err(_) => {
                eprintln!("Error: --temperature expects a float (e.g. 0.2), got '{s}'.");
                std::process::exit(2);
            }
        },
    };

    // Per-task iteration cap. `--max-steps` overrides `ATHEN_MAX_STEPS`. 0 ⇒ unlimited
    // (mapped to `u32::MAX`). Default 50 matches the interactive REPL build.
    let max_steps_arg = match extract_flag(&args, "--max-steps") {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("Error: {msg}");
            eprintln!(
                "Usage: athen-cli --prompt <PROMPT> [--profile <ID>] [--family <ID>] [--max-steps <N>]"
            );
            std::process::exit(2);
        }
    };
    let max_steps_str = max_steps_arg.or_else(|| {
        std::env::var("ATHEN_MAX_STEPS")
            .ok()
            .filter(|s| !s.is_empty())
    });
    let max_steps = match max_steps_str.as_deref() {
        None => 50u32,
        Some(s) => match s.parse::<u32>() {
            Ok(0) => u32::MAX,
            Ok(n) => n,
            Err(_) => {
                eprintln!("Error: --max-steps expects a non-negative integer, got '{s}'.");
                std::process::exit(2);
            }
        },
    };

    if let Some(prompt) = prompt_arg {
        match run_headless(prompt, profile_arg, family, temperature, max_steps).await {
            Ok(()) => std::process::exit(0),
            Err((code, msg)) => {
                eprintln!("{msg}");
                std::process::exit(code);
            }
        }
    }
    // No --prompt → fall through to REPL (any --profile is intentionally
    // ignored here; REPL profile support is future work).

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
                    AuthType::ApiKey(key) if !key.is_empty() && !key.starts_with("${") => {
                        key.clone()
                    }
                    _ => {
                        eprintln!("Error: DEEPSEEK_API_KEY environment variable not set.");
                        eprintln!("Export it before running:  export DEEPSEEK_API_KEY=\"sk-...\"");
                        std::process::exit(1);
                    }
                },
                None => {
                    eprintln!("Error: DEEPSEEK_API_KEY environment variable not set.");
                    eprintln!("Export it before running:  export DEEPSEEK_API_KEY=\"sk-...\"");
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
                .max_steps(50)
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
                                println!(
                                    "{}",
                                    serde_json::to_string_pretty(output).unwrap_or_default()
                                );
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
