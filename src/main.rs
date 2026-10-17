mod banner;
mod ca;
mod check;
mod child;
mod cli;
mod config;
mod daemon;
mod date;
mod der;
mod dirs;
mod log;
mod mapping;
mod net;
mod proxy;
mod server;
mod tls;
mod trust;
mod upstream;
mod watch;

use std::process::ExitCode;

use clap::Parser;
use nix::sys::signal::Signal;
use tokio::signal::unix::{SignalKind, signal};

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::Cli;
use crate::config::Config;
use crate::server::Server;

fn main() -> ExitCode {
    // ferry listens before its signal handlers are installed in `run`, and
    // SIGHUP and SIGUSR1 would kill it until then. Hold them until it's
    // ready; one that arrives meanwhile is delivered then, not lost.
    held_signals().thread_block().expect("can block signals");
    let cli = cli::Cli::parse();
    if let Some(shell) = cli.completions {
        cli::Cli::print_completions(shell);
        return ExitCode::SUCCESS;
    }
    if cli.daemonize && !cli.command.is_empty() {
        return fail(vec![format!(
            "can't run `{}` with -d: running a command needs ferry in the foreground",
            child::display(&cli.command)
        )]);
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
    let mut plan = plan;
    plan.trust_port = plan
        .listeners
        .iter()
        .find(|s| s.kind == server::Kind::Trust)
        .and_then(|s| sockets.get(&s.key))
        .and_then(|socket| socket.local_addr().ok())
        .map(|addr| addr.port());

    let pid_path = config.pid_file.clone();
    if let Some(path) = &pid_path
        && let Err(e) = daemon::check_pid_file(path)
    {
        return fail(vec![e]);
    }
    if let Some(ca) = &plan.local_ca
        && let Some(why) = &ca.created
    {
        tracing::info!(
            "created ferry's local CA ({why}): {}",
            ca.paths.root.cert.display()
        );
    }
    // With a command, the banner waits until the app is listening, so it
    // lands below the app's own startup output (and survives a dev server
    // clearing the screen).
    let banner = match cli.command.is_empty() {
        true => {
            banner::show(&config, &plan, cli.daemonize);
            None
        }
        false => {
            banner::log_listeners(&config);
            banner::text(&config, &plan, false)
        }
    };

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
    runtime.block_on(run(cli, cwd, config, plan, sockets, banner))
}

/// Why the main loop ended.
enum Stop {
    Signal(Signal),
    /// The command ferry was running exited.
    Exited(std::io::Result<std::process::ExitStatus>),
}

async fn run(
    cli: Cli,
    cwd: PathBuf,
    config: Config,
    plan: server::Plan,
    sockets: server::Sockets,
    banner: Option<String>,
) -> ExitCode {
    // Handle signals before serving, so none arrive unhandled.
    let mut sigterm = signal(SignalKind::terminate()).expect("can install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("can install SIGINT handler");
    let mut sighup = signal(SignalKind::hangup()).expect("can install SIGHUP handler");
    let mut sigusr1 = signal(SignalKind::user_defined1()).expect("can install SIGUSR1 handler");
    held_signals()
        .thread_unblock()
        .expect("can unblock signals");

    let mut server = Server::new();
    let mut issued = issued(&plan);
    let mut access_log = plan.log.clone();
    if let Err(e) = server.apply(plan.listeners, sockets) {
        return fail(vec![e.to_string()]);
    }

    let mut child = match cli.command.is_empty() {
        true => None,
        false => match child::spawn(&cli.command) {
            Ok(c) => Some(c),
            Err(e) => return fail(vec![e]),
        },
    };
    if child.is_some() {
        let upstreams: Vec<_> = config
            .listeners
            .iter()
            .map(|l| l.mapping.upstream.clone())
            .collect();
        tokio::spawn(child::watch_startup(upstreams, banner));
    }

    let mut watcher = watch::FileWatcher::new();
    watcher.set_files(watch::files(&config));

    let mut config = config;
    // Reissue the local CA's server certificate when this machine's names or
    // addresses change (a laptop joining another network), or it's due.
    let mut reissue_check = tokio::time::interval(Duration::from_secs(30));
    reissue_check.tick().await;

    let stop = loop {
        let reason = tokio::select! {
            _ = sigterm.recv() => break Stop::Signal(Signal::SIGTERM),
            _ = sigint.recv() => break Stop::Signal(Signal::SIGINT),
            status = wait_for(&mut child) => break Stop::Exited(status),
            _ = sighup.recv() => "SIGHUP".to_string(),
            _ = sigusr1.recv() => {
                reopen_logs(&access_log);
                continue;
            }
            changed = watcher.changed() => {
                let names: Vec<String> = changed.iter().map(|p| p.display().to_string()).collect();
                format!("{} changed", names.join(", "))
            }
            _ = reissue_check.tick() => {
                let reason = issued
                    .as_ref()
                    .and_then(|(names, not_after)| ca::LocalCa::reissue_reason(names, *not_after));
                match reason {
                    Some(reason) => reason.to_string(),
                    None => continue,
                }
            }
        };

        match reload(&cli, &cwd, &mut server) {
            Ok((new_config, plan)) => {
                tracing::info!(
                    "reloaded configuration ({reason}): {} listener(s)",
                    new_config.listeners.len()
                );
                issued = plan.issued;
                access_log = plan.log;
                watcher.set_files(watch::files(&new_config));
                config = new_config;
            }
            Err(errors) => {
                tracing::error!(
                    "not reloading ({reason}); keeping the current configuration:\n  {}",
                    errors.join("\n  ")
                );
            }
        }
    };

    let grace = config.advanced.shutdown_grace;
    let code = match (stop, &mut child) {
        (Stop::Exited(status), _) => {
            child::started();
            let command = child::display(&cli.command);
            match status {
                Ok(status) => {
                    tracing::info!("`{command}` exited ({status})");
                    child::exit_code(status)
                }
                Err(e) => {
                    tracing::error!("can't tell how `{command}` exited: {e}");
                    1
                }
            }
        }
        (Stop::Signal(sig), Some(c)) => {
            child::forward(c, sig);
            child::stop(c, grace).await.map_or(1, child::exit_code)
        }
        (Stop::Signal(_), None) => 0,
    };

    tracing::info!("shutting down");
    server
        .shutdown(grace, async {
            sigint.recv().await;
        })
        .await;
    ExitCode::from(code)
}

/// Waits for the command ferry runs to exit; forever if there isn't one.
async fn wait_for(
    child: &mut Option<tokio::process::Child>,
) -> std::io::Result<std::process::ExitStatus> {
    match child {
        Some(child) => child.wait().await,
        None => std::future::pending().await,
    }
}

/// The names the local CA's server certificate was issued for and when it
/// expires, if it's in use.
type Issued = Option<(std::collections::BTreeSet<String>, i64)>;

fn issued(plan: &server::Plan) -> Issued {
    plan.local_ca
        .as_ref()
        .map(|ca| (ca.wanted.clone(), ca.not_after))
}

struct Reloaded {
    issued: Issued,
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
        issued: issued(&plan),
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

/// Signals held during startup; see `main`.
fn held_signals() -> nix::sys::signal::SigSet {
    use nix::sys::signal::{SigSet, Signal};
    let mut set = SigSet::empty();
    set.add(Signal::SIGHUP);
    set.add(Signal::SIGUSR1);
    set
}

fn fail(errors: Vec<String>) -> ExitCode {
    for e in errors {
        eprintln!("error: {e}");
    }
    ExitCode::FAILURE
}
