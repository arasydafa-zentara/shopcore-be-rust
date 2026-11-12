// Rows fetched with `query_as` are plain tuples; naming each one would only add noise.
#![allow(clippy::type_complexity)]

mod auth;
mod config;
mod db;
mod error;
mod jobs;
mod manage;
mod routes;
mod services;
mod state;
mod util;

use std::{sync::Arc, time::Duration};

use clap::{Parser, Subcommand};

use crate::{config::Config, state::AppState};

#[derive(Parser)]
#[command(name = "shopcore", about = "ShopCore order, wallet and reporting backend")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the HTTP API (default).
    Serve,
    /// Exit 0 if the local API answers /health (used by the container healthcheck).
    Healthcheck,
    /// Maintenance commands: init-db, seed, import-products <csv>, export-sales <file>, purge-sessions.
    Manage {
        cmd: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,sqlx=warn".into()),
        )
        .init();
    let cfg = Config::from_env();
    let code = match Cli::parse().command.unwrap_or(Cmd::Serve) {
        Cmd::Serve => serve(cfg).await,
        Cmd::Healthcheck => healthcheck(&cfg).await,
        Cmd::Manage { cmd, args } => manage::run(&cfg, cmd.as_deref(), &args).await,
    };
    std::process::exit(code);
}

async fn healthcheck(cfg: &Config) -> i32 {
    let url = format!("http://127.0.0.1:{}/health", cfg.port);
    match reqwest::Client::new()
        .get(url)
        .timeout(Duration::from_secs(3))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => 0,
        _ => 1,
    }
}

async fn serve(cfg: Config) -> i32 {
    if cfg.admin_password == "admin123" || cfg.bank_secret == "bank-shared-secret-2019" {
        tracing::warn!("default ADMIN_PASSWORD / BANK_SECRET in use: set your own values outside of testing");
    }
    if !cfg.notify_block_private {
        tracing::warn!(
            "NOTIFY_BLOCK_PRIVATE is off: notify_url may point at private addresses; enable it in production"
        );
    }
    let pool = match db::connect(&cfg).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "cannot connect to the database");
            return 1;
        }
    };
    if let Err(e) = db::init_db(&pool).await {
        tracing::error!(error = %e, "migration failed");
        return 1;
    }
    if let Err(e) = db::seed(&pool, &cfg).await {
        tracing::error!(error = %e, "seeding failed");
        return 1;
    }
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("http client");
    let notifier = jobs::Notifier::start(&cfg);
    let port = cfg.port;
    let state = AppState {
        pool,
        cfg: Arc::new(cfg),
        http,
        notifier,
    };
    let app = routes::router(state);
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "cannot bind port");
            return 1;
        }
    };
    tracing::info!(port, "shopcore listening");
    let served = axum::serve(listener, app).with_graceful_shutdown(shutdown()).await;
    if let Err(e) = served {
        tracing::error!(error = %e, "server error");
        return 1;
    }
    0
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}
