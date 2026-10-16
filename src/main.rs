// Allowed while v0.1 is being built out; remove once everything is wired up.
#![allow(dead_code)]

mod cli;
mod config;
mod log;
mod mapping;
mod proxy;
mod tls;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    let config = match config::Config::load(&cli) {
        Ok(c) => c,
        Err(errors) => {
            for e in errors {
                eprintln!("error: {e}");
            }
            return ExitCode::FAILURE;
        }
    };

    // TODO(#5): serve instead of describing the plan.
    for l in &config.listeners {
        let cert = match l.cert_key(&config) {
            Some(ck) => ck.cert.display().to_string(),
            None => "self-signed".into(),
        };
        eprintln!(
            "  https :{}  ->  {}  ({cert})",
            l.mapping.listen_display(),
            l.mapping.upstream
        );
    }
    eprintln!("ferry: proxying is not implemented yet");
    ExitCode::SUCCESS
}
