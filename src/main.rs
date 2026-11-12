//! copilot-api-proxy - A reverse proxy server for GitHub Copilot API

use anyhow::Result;
use clap::{Parser, Subcommand};
use copilot_api_proxy::{auth, config, policy_config::PolicyConfig, server};
use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLabel, ServiceLevel, ServiceManager, ServiceStartCtx,
    ServiceStatus, ServiceStatusCtx, ServiceStopCtx, ServiceUninstallCtx,
};
use std::ffi::OsString;
use std::path::PathBuf;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(name = "copilot-api-proxy")]
#[command(about = "A reverse proxy server for GitHub Copilot API")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run OAuth authentication flow
    Auth,
    /// Start the proxy server
    Server {
        #[arg(long, default_value = "0.0.0.0")]
        host: String,
        #[arg(short, long, default_value = "9876")]
        port: u16,
        #[arg(long, default_value = "info")]
        log_level: String,
        /// Require a client bearer token (or set COPILOT_PROXY_API_KEY)
        #[arg(long, value_name = "TOKEN")]
        api_key: Option<String>,
        /// Read the client bearer token from a file instead of the environment
        #[arg(long, value_name = "PATH", conflicts_with = "api_key")]
        api_key_file: Option<PathBuf>,
        /// Load model rules and the admin password from a private JSON file
        #[arg(long, value_name = "PATH")]
        model_policy_file: Option<PathBuf>,
    },
    /// Manage the system service
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install the service daemon
    Install {
        /// IP address / host the service should bind to
        #[arg(long, default_value = "0.0.0.0")]
        host: String,
        #[arg(short, long, default_value = "9876")]
        port: u16,
        /// Require a client bearer token (or set COPILOT_PROXY_API_KEY)
        #[arg(long, value_name = "TOKEN")]
        api_key: Option<String>,
    },
    /// Uninstall the service daemon
    Uninstall,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    match cli.command {
        Commands::Auth => run_auth().await,
        Commands::Server {
            host,
            port,
            log_level,
            api_key,
            api_key_file,
            model_policy_file,
        } => {
            run_server(
                &host,
                port,
                &log_level,
                api_key,
                api_key_file,
                model_policy_file,
            )
            .await
        }
        Commands::Service { action } => match action {
            ServiceAction::Install {
                host,
                port,
                api_key,
            } => install_service(&host, port, api_key),
            ServiceAction::Uninstall => uninstall_service(),
        },
    }
}

async fn run_auth() -> Result<()> {
    init_tracing("copilot_api_proxy=debug");

    config::ensure_token_dir()?;

    let github_token = auth::run_device_flow().await?;

    let token_path = config::token_path();
    config::write_token(&token_path, &github_token)?;

    println!("\nAuthentication successful!");
    println!("Token saved to: {}\n", token_path.display());

    Ok(())
}

async fn run_server(
    host: &str,
    port: u16,
    log_level: &str,
    api_key: Option<String>,
    api_key_file: Option<PathBuf>,
    model_policy_file: Option<PathBuf>,
) -> Result<()> {
    let filter = format!("copilot_api_proxy={},tower_http={}", log_level, log_level);
    init_tracing(&filter);

    let api_key = match api_key_file {
        Some(path) => Some(config::load_proxy_api_key_file(&path)?),
        None => api_key,
    };
    let configuration = PolicyConfig::load(model_policy_file.as_deref())?;
    let state = server::AppState::new_with_policy_config(api_key, configuration).await?;
    let app = server::create_router(state);

    let listener = tokio::net::TcpListener::bind(format!("{}:{}", host, port)).await?;
    tracing::info!("Server listening on http://{}:{}", host, port);

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await?;

    Ok(())
}

fn init_tracing(default_filter: &str) {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
}

const SERVICE_LABEL: &str = "wxplus.copilot-api-proxy";

fn install_service(host: &str, port: u16, api_key: Option<String>) -> Result<()> {
    let api_key = config::resolve_proxy_api_key(api_key)?;
    let configuration = PolicyConfig::load(None)?;
    let label: ServiceLabel = SERVICE_LABEL.parse()?;
    let mut manager = <dyn ServiceManager>::native()?;

    let program = std::env::current_exe()?;
    let mut args = vec![
        OsString::from("server"),
        OsString::from("--host"),
        OsString::from(host),
        OsString::from("--port"),
        OsString::from(port.to_string()),
    ];

    manager.set_level(ServiceLevel::User)?;
    if matches!(
        manager.status(ServiceStatusCtx {
            label: label.clone(),
        })?,
        ServiceStatus::Running
    ) {
        manager.stop(ServiceStopCtx {
            label: label.clone(),
        })?;
    }

    if let Some(api_key) = api_key {
        config::ensure_token_dir()?;
        let path = config::proxy_api_key_path();
        config::write_token(&path, &api_key)?;
        args.push(OsString::from("--api-key-file"));
        args.push(path.into_os_string());
    }
    if configuration.is_configured() {
        let path = configuration.save_for_service()?;
        args.push(OsString::from("--model-policy-file"));
        args.push(path.into_os_string());
    }

    manager.install(ServiceInstallCtx {
        label: label.clone(),
        program,
        args,
        contents: None,
        username: None,
        working_directory: None,
        environment: None,
        autostart: true,
        restart_policy: RestartPolicy::OnFailure {
            delay_secs: Some(10),
        },
    })?;
    manager.start(ServiceStartCtx {
        label: label.clone(),
    })?;

    println!("Service installed successfully as '{}'", SERVICE_LABEL);
    println!("The service will listen on http://{}:{}", host, port);
    println!("The service will start automatically on system boot.");
    println!("The service has been started.");

    Ok(())
}

fn uninstall_service() -> Result<()> {
    let label: ServiceLabel = SERVICE_LABEL.parse()?;
    let mut manager = <dyn ServiceManager>::native()?;
    manager.set_level(ServiceLevel::User)?;

    manager.uninstall(ServiceUninstallCtx { label })?;

    println!("Service '{}' uninstalled successfully.", SERVICE_LABEL);

    Ok(())
}
