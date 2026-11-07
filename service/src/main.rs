//! `kfm-ladder`: the ladder service binary (docs/ladder-api.md; architecture
//! v0.63 §11 "M7 step 3 design"). Configuration is the environment only.

use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use kfm_ladder::config::{database_url_from, Config};
use kfm_ladder::{db, AppState};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Kung Fu Monkeys ladder: accounts, tokens, teams, submissions, matches and
/// ratings over Postgres. Every setting is an environment variable
/// (service/README.md); RUST_LOG filters the log (default: info).
#[derive(Parser, Debug)]
#[command(name = "kfm-ladder", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Apply pending migrations, then serve the API and the portal on
    /// KFM_LADDER_LISTEN (Caddy terminates TLS in front of it).
    Serve,
    /// Apply pending migrations and exit (needs DATABASE_URL only).
    Migrate,
    /// Run a worker (not implemented yet: step 3b). A worker reads only its
    /// own variables (`WorkerConfig`): no session key, OAuth app, public URL
    /// or listen address.
    Worker {
        #[command(subcommand)]
        role: WorkerRole,
    },
}

#[derive(Subcommand, Debug, Clone, Copy)]
enum WorkerRole {
    /// Claim build jobs: a submission's source into a module, in the sandbox.
    Build,
    /// Claim match jobs: one kfm_arena run, its result and replay.
    Match,
}

/// How long `serve` and `migrate` wait for Postgres to accept connections.
const DB_PATIENCE: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Worker { role } => {
            let name = match role {
                WorkerRole::Build => "build",
                WorkerRole::Match => "match",
            };
            eprintln!("kfm-ladder: worker {name}: not implemented (M7 step 3b)");
            ExitCode::from(2)
        }
        Command::Migrate => {
            let url = match database_url_from(|k| std::env::var(k).ok()) {
                Ok(url) => url,
                Err(err) => {
                    eprintln!("kfm-ladder: {err}");
                    return ExitCode::from(2);
                }
            };
            #[cfg(feature = "sentry")]
            let _sentry = init_sentry(std::env::var("SENTRY_DSN").ok());
            init_logging();
            run(async move {
                let pool = db::connect(&url, DB_PATIENCE).await?;
                db::migrate(&pool).await?;
                tracing::info!("migrations applied");
                Ok(())
            })
        }
        Command::Serve => {
            let config = match Config::from_env() {
                Ok(config) => config,
                Err(err) => {
                    eprintln!("kfm-ladder: {err}");
                    return ExitCode::from(2);
                }
            };
            // Sentry starts before the runtime so it sees every thread.
            #[cfg(feature = "sentry")]
            let _sentry = init_sentry(config.sentry_dsn.clone());
            init_logging();
            run(serve(config))
        }
    }
}

#[cfg(feature = "sentry")]
fn init_sentry(dsn: Option<String>) -> Option<sentry::ClientInitGuard> {
    dsn.filter(|dsn| !dsn.is_empty()).map(|dsn| {
        let mut options = sentry::ClientOptions::default();
        options.release = sentry::release_name!();
        sentry::init((dsn, options))
    })
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry()
        .with(filter)
        // Colour only on a terminal: journald gets plain lines.
        .with(tracing_subscriber::fmt::layer().with_ansi(std::io::stdout().is_terminal()));
    // Error events become Sentry events, info and warnings their breadcrumbs
    // (a no-op without SENTRY_DSN).
    #[cfg(feature = "sentry")]
    let registry = {
        use tracing_subscriber::filter::LevelFilter;
        use tracing_subscriber::Layer;
        registry
            .with(sentry_tracing::ErrorLayer::new().with_filter(LevelFilter::ERROR))
            .with(sentry_tracing::BreadcrumbLayer::new().with_filter(LevelFilter::INFO))
    };
    registry.init();
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

fn run(task: impl std::future::Future<Output = Result<(), BoxError>>) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("kfm-ladder: cannot start the runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(task) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "kfm-ladder failed");
            eprintln!("kfm-ladder: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn serve(config: Config) -> Result<(), BoxError> {
    tracing::info!(?config, "configuration");
    let pool = db::connect(&config.database_url, DB_PATIENCE).await?;
    db::migrate(&pool).await?;
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    tracing::info!(listen = %listener.local_addr()?, public_url = %config.public_url, "kfm-ladder listening");
    let state = AppState::new(pool, config);
    kfm_ladder::serve(state, listener, shutdown_signal()).await?;
    Ok(())
}

/// Ctrl-C or SIGTERM (systemd's stop).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = term => {}
    }
    tracing::info!("shutting down");
}
