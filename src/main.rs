use omnirag::config;
use omnirag::hashing;
use omnirag::redis_client;
use omnirag::sync;
use omnirag::web;

use clap::{Parser, Subcommand};
use config::AppConfig;
use hashing::generate_redis_key;
use std::path::PathBuf;
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "omnirag", version, about = "OmniRAG — Local file ingestion & sync engine for Open WebUI")]
struct Cli {
    /// Redis connection URL (overrides REDIS_URL env var and .env)
    #[arg(long, env = "REDIS_URL")]
    redis_url: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Boot the axum Web UI server on port 3000
    Serve,

    /// Execute full sync: Phase 0 (Reconciliation) + Phase 1 (Ingestion) + Phase 2 (Orphan Cleanup)
    Sync,

    /// Set context text for a file in Redis
    SetContext {
        /// Path to the local file
        file_path: PathBuf,
        /// The context string to store
        context_string: String,
    },

    /// Get formatted context for a file from Redis
    GetContext {
        /// Path to the local file
        file_path: PathBuf,
    },
}

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Load .env if present (errors are fine — file may not exist)
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    // Determine Redis URL: CLI flag > env var > .env > default
    let redis_url = cli
        .redis_url
        .or_else(|| std::env::var("REDIS_URL").ok())
        .unwrap_or_else(|| "redis://127.0.0.1:6379/0".to_string());

    info!("Connecting to Redis at {}", redis_url);

    // Connect to Redis
    let client = match redis::Client::open(redis_url.as_str()) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create Redis client: {}", e);
            std::process::exit(1);
        }
    };

    let mut con = match client.get_multiplexed_async_connection().await {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to connect to Redis: {}", e);
            std::process::exit(1);
        }
    };

    // Load Lua functions into Redis on startup
    if let Err(e) = redis_client::load_functions(&mut con).await {
        error!("Failed to load Redis functions: {}", e);
        std::process::exit(1);
    }

    match cli.command {
        Commands::Serve => {
            if let Err(e) = web::serve(con).await {
                error!("Web server error: {}", e);
                std::process::exit(1);
            }
        }

        Commands::Sync => {
            let mut config = match AppConfig::load_from_redis(&mut con).await {
                Ok(c) => c,
                Err(e) => {
                    error!("Failed to load config from Redis: {}", e);
                    std::process::exit(1);
                }
            };

            // Default to /rag if not explicitly set
            if config.target_directory.is_empty() {
                config.target_directory = "/rag".to_string();
            }

            if let Err(e) = sync::run_sync(&mut con, &config).await {
                error!("Sync failed: {}", e);
                std::process::exit(1);
            }
        }

        Commands::SetContext {
            file_path,
            context_string,
        } => {
            let key = generate_redis_key(&file_path);
            info!("Setting context for key: {}", key);

            let result: Result<(), redis::RedisError> = redis::cmd("HSET")
                .arg(&key)
                .arg("context_text")
                .arg(&context_string)
                .arg("context_dirty")
                .arg("true")
                .query_async(&mut con)
                .await;

            match result {
                Ok(_) => info!("Context set successfully for '{}' (marked dirty for re-sync)", file_path.display()),
                Err(e) => {
                    error!("Failed to set context: {}", e);
                    std::process::exit(1);
                }
            }
        }

        Commands::GetContext { file_path } => {
            let key = generate_redis_key(&file_path);

            match redis_client::fcall_get_formatted_context(&mut con, &key).await {
                Ok(output) => {
                    if output.is_empty() {
                        println!("(no context set for this file)");
                    } else {
                        print!("{}", output);
                    }
                }
                Err(e) => {
                    error!("Failed to get context: {}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}
