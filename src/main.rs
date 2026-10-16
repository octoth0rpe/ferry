mod banner;
mod check;
mod cli;
mod config;
mod daemon;
mod log;
mod mapping;
mod net;
mod proxy;
mod selfsigned;
mod server;
mod tls;
mod upstream;
mod watch;

use std::process::ExitCode;

use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::Cli;
use crate::config::Config;
use crate::server::Server;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    if let Some(shell) = cli.completions {
        cli::Cli::print_completions(shell);
        return ExitCode::SUCCESS;
    }
    // Relative paths on the command line are resolved against this, even
    // after daemonizing changes directory or on a later reload.
    let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
    let config = match Config::load_in(&cli, &cwd) {
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

    let pid_path = cli
        .pid_file
        .as_ref()
        .map(|p| config::absolute(&cwd.join(p)));
    if let Some(path) = &pid_path
        && let Err(e) = daemon::check_pid_file(path)
    {
        return fail(vec![e]);
    }
    if let Some(ss) = &plan.self_signed
        && let Some(why) = &ss.generated
    {
        tracing::info!(
            "generated a self-signed certificate ({why}): {}",
            ss.paths.cert.display()
        );
    }
    banner::show(&config, &plan);

    let write_pid = || pid_path.as_deref().map(daemon::write_pid_file).transpose();
    let _pid_file = if cli.daemonize {
        if config.access_log == config::AccessLog::Stdout && !config.quiet {
            eprintln!(
                "warning: running in the background discards the access log; use --access-log FILE"
            );
        }
        if config.error_log == config::ErrorLog::Stderr {
            eprintln!(
                "warning: running in the background discards the error log; use --error-log FILE"
            );
        }
        daemon::daemonize(write_pid)
    } else {
        match write_pid() {
            Ok(p) => p,
            Err(e) => return fail(vec![e]),
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("can start the tokio runtime");
    runtime.block_on(run(cli, cwd, config, plan, sockets))
}

async fn run(
    cli: Cli,
    cwd: PathBuf,
    config: Config,
    plan: server::Plan,
    sockets: server::Sockets,
) -> ExitCode {
    // Handle signals before serving, so none arrive unhandled.
    let mut sigterm = signal(SignalKind::terminate()).expect("can install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("can install SIGINT handler");
    let mut sighup = signal(SignalKind::hangup()).expect("can install SIGHUP handler");
    let mut sigusr1 = signal(SignalKind::user_defined1()).expect("can install SIGUSR1 handler");

    let mut server = Server::new();
    let mut uses_self_signed = plan.self_signed.is_some();
    let mut access_log = plan.log.clone();
    if let Err(e) = server.apply(plan.listeners, sockets) {
        return fail(vec![e.to_string()]);
    }

    let mut watcher = match watch::FileWatcher::new() {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::warn!("can't watch files for changes ({e}); reload with SIGHUP instead");
            None
        }
    };
    if let Some(w) = &mut watcher {
        w.set_files(watch::files(&config));
    }

    let mut config = config;
    // Long-running ferries renew their self-signed certificate in place.
    let mut renewal_check = tokio::time::interval(Duration::from_secs(12 * 3600));
    renewal_check.tick().await;

    loop {
        let reason = tokio::select! {
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
            _ = sighup.recv() => "SIGHUP".to_string(),
            _ = sigusr1.recv() => {
                reopen_logs(&access_log);
                continue;
            }
            changed = async { watcher.as_mut().unwrap().changed().await }, if watcher.is_some() => {
                let names: Vec<String> = changed.iter().map(|p| p.display().to_string()).collect();
                format!("{} changed", names.join(", "))
            }
            _ = renewal_check.tick() => {
                if !(uses_self_signed && selfsigned::expires_soon()) {
                    continue;
                }
                "self-signed certificate expires soon".to_string()
            }
        };

        match reload(&cli, &cwd, &mut server) {
            Ok((new_config, plan)) => {
                tracing::info!(
                    "reloaded configuration ({reason}): {} listener(s)",
                    new_config.listeners.len()
                );
                uses_self_signed = plan.uses_self_signed;
                access_log = plan.log;
                if let Some(w) = &mut watcher {
                    w.set_files(watch::files(&new_config));
                }
                config = new_config;
            }
            Err(errors) => {
                tracing::error!(
                    "not reloading ({reason}); keeping the current configuration:\n  {}",
                    errors.join("\n  ")
                );
            }
        }
    }

    tracing::info!("shutting down");
    server
        .shutdown(config.advanced.shutdown_grace, async {
            sigint.recv().await;
        })
        .await;
    ExitCode::SUCCESS
}

struct Reloaded {
    uses_self_signed: bool,
    log: std::sync::Arc<log::AccessLogger>,
}

/// Loads, checks and applies the configuration again. On any error the
/// running configuration is left untouched.
fn reload(cli: &Cli, cwd: &Path, server: &mut Server) -> Result<(Config, Reloaded), Vec<String>> {
    let config = Config::load_in(cli, cwd)?;
    let plan = server::build(&config)?;
    for w in &plan.warnings {
        tracing::warn!("{w}");
    }
    let (sockets, warnings) = server::bind(&plan.listeners, |key| server.is_running(key))?;
    for w in warnings {
        tracing::warn!("{w}");
    }
    log::set_error_log(&config.error_log)
        .map_err(|e| vec![format!("can't open error log: {e}")])?;
    for l in &config.listeners {
        tracing::info!(
            "listening on {} -> {}",
            l.mapping.listen_display(),
            l.mapping.upstream
        );
    }
    let reloaded = Reloaded {
        uses_self_signed: plan.self_signed.is_some(),
        log: plan.log.clone(),
    };
    server
        .apply(plan.listeners, sockets)
        .map_err(|e| vec![e.to_string()])?;
    Ok((config, reloaded))
}

/// Reopens log files, for logrotate.
fn reopen_logs(access_log: &log::AccessLogger) {
    if let Err(e) = access_log.reopen() {
        tracing::error!("can't reopen access log: {e}");
    }
    if let Err(e) = log::reopen_error_log() {
        tracing::error!("can't reopen error log: {e}");
    }
    tracing::info!("reopened log files");
}

fn fail(errors: Vec<String>) -> ExitCode {
    for e in errors {
        eprintln!("error: {e}");
    }
    ExitCode::FAILURE
}
