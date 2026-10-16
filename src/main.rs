// Allowed while v0.1 is being built out; remove once everything is wired up.
#![allow(dead_code)]

mod banner;
mod check;
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
    let config = match Config::load(&cli) {
        Ok(c) => c,
        Err(errors) => {
            if cli.test {
                println!("configuration: {} error(s)", errors.len());
            }
            return fail(errors);
        }
    };
    if cli.print_config {
        print!("{}", config.to_toml());
        return ExitCode::SUCCESS;
    }
    if cli.test {
        return match check::check(&config).print(&config) {
            true => ExitCode::SUCCESS,
            false => ExitCode::FAILURE,
        };
    }
    if let Err(e) = log::set_error_log(&config.error_log) {
        let config::ErrorLog::File(path) = &config.error_log else {
            unreachable!("only files can fail to open")
        };
        return fail(vec![format!(
            "can't open error log {}: {e}",
            path.display()
        )]);
    }
    log::init_error_log();
    let plan = match server::build(&config) {
        Ok(plan) => plan,
        Err(errors) => return fail(errors),
    };
    for w in &plan.warnings {
        tracing::warn!("{w}");
    }
    let sockets = match server::bind(&plan.listeners, |_| false) {
        Ok((sockets, warnings)) => {
            for w in warnings {
                tracing::warn!("{w}");
            }
            sockets
        }
        Err(errors) => return fail(errors),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("can start the tokio runtime");
    runtime.block_on(run(config, plan, sockets))
}

async fn run(config: Config, plan: server::Plan, sockets: server::Sockets) -> ExitCode {
    let mut server = Server::new();
    if let Some(ss) = &plan.self_signed
        && let Some(why) = &ss.generated
    {
        tracing::info!(
            "generated a self-signed certificate ({why}): {}",
            ss.paths.cert.display()
        );
    }
    banner::show(&config, &plan);
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
