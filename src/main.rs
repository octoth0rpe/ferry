// Allowed while v0.1 is being built out; remove once everything is wired up.
#![allow(dead_code)]

mod cli;
mod config;
mod log;
mod mapping;
mod net;
mod proxy;
mod selfsigned;
mod server;
mod tls;
mod upstream;

use std::process::ExitCode;

use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};

use crate::config::Config;
use crate::server::Server;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FERRY_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = match Config::load(&cli) {
        Ok(c) => c,
        Err(errors) => return fail(errors),
    };
    let plan = match server::build(&config) {
        Ok(plan) => plan,
        Err(errors) => return fail(errors),
    };
    for w in &plan.warnings {
        tracing::warn!("{w}");
    }
    let sockets = match server::bind(&plan.listeners, |_| false) {
        Ok(s) => s,
        Err(errors) => return fail(errors),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("can start the tokio runtime");
    runtime.block_on(run(config, plan, sockets))
}

async fn run(
    config: Config,
    plan: server::Plan,
    sockets: std::collections::HashMap<server::ListenKey, std::net::TcpListener>,
) -> ExitCode {
    let mut server = Server::new();
    if let Some(ss) = &plan.self_signed {
        match &ss.generated {
            Some(why) => tracing::info!(
                "generated a self-signed certificate ({why}): {}",
                ss.paths.cert.display()
            ),
            None => tracing::info!("using self-signed certificate {}", ss.paths.cert.display()),
        }
    }
    // TODO(#17): a proper banner.
    for l in &config.listeners {
        tracing::info!(
            "https :{} -> {}",
            l.mapping.listen_display(),
            l.mapping.upstream
        );
    }
    if let Err(e) = server.apply(plan.listeners, sockets) {
        return fail(vec![e.to_string()]);
    }

    let mut sigterm = signal(SignalKind::terminate()).expect("can install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("can install SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
    tracing::info!("shutting down");
    server
        .shutdown(config.advanced.shutdown_grace, async {
            sigint.recv().await;
        })
        .await;
    ExitCode::SUCCESS
}

fn fail(errors: Vec<String>) -> ExitCode {
    for e in errors {
        eprintln!("error: {e}");
    }
    ExitCode::FAILURE
}
